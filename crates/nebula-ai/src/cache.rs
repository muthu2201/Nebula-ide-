//! Prompt cache breakpoint placement.
//!
//! Anthropic's prompt cache is opt-in through explicit `cache_control`
//! breakpoints, and the guidance is to prefer them over automatic caching for
//! agent loops. The reason is that automatic caching cannot know which prefix of
//! your prompt is stable, whereas the caller does.
//!
//! The rule that makes caching work: **a breakpoint caches everything before
//! it**, so it must sit after content that does not change between turns. Put it
//! after something that changes every turn and every request re-writes the cache
//! at 1.25× or 2× the input rate — strictly worse than not caching at all, and
//! silently so. That failure mode is why [`crate::provider::Usage`] exposes a
//! hit rate.

use serde::{Deserialize, Serialize};

/// How long a cache entry lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheTtl {
    /// Five minutes. Write costs 1.25× the input rate.
    ///
    /// The right choice for anything that changes within a session — the repo
    /// map, the current file's contents.
    FiveMinutes,
    /// One hour. Write costs 2× the input rate.
    ///
    /// Only worth it for content stable across a long session and re-read many
    /// times: the system prompt and the tool definitions. The write costs 60%
    /// more than the five-minute variant, so it needs enough hits to repay that.
    OneHour,
}

impl CacheTtl {
    /// The wire value.
    pub const fn as_str(&self) -> &'static str {
        match self {
            CacheTtl::FiveMinutes => "5m",
            CacheTtl::OneHour => "1h",
        }
    }

    /// The multiplier applied to the input rate when writing.
    pub const fn write_multiplier(&self) -> f64 {
        match self {
            CacheTtl::FiveMinutes => 1.25,
            CacheTtl::OneHour => 2.0,
        }
    }

    /// The `cache_control` object for this TTL.
    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({ "type": "ephemeral", "ttl": self.as_str() })
    }

    /// How many hits are needed before writing at this TTL pays for itself.
    ///
    /// A write costs `multiplier` and each subsequent read costs 0.1, against
    /// 1.0 for an uncached read. So `n` reads cost `multiplier + 0.1n` cached
    /// versus `n` uncached, and caching wins once `n > multiplier / 0.9`.
    pub fn breakeven_hits(&self) -> usize {
        (self.write_multiplier() / 0.9).ceil() as usize
    }
}

/// Where a breakpoint sits in the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheBreakpoint {
    /// After the system prompt.
    System(CacheTtl),
    /// After the tool definitions.
    Tools(CacheTtl),
    /// After the message at this index.
    Message(usize, CacheTtl),
}

/// Plan cache breakpoints for a conversation.
///
/// The strategy is the one that survives an agent loop:
///
/// 1. The system prompt is identical every turn, so it gets a one-hour
///    breakpoint if the session is expected to be long.
/// 2. Tool definitions are equally stable, and sit right after the system
///    prompt.
/// 3. One breakpoint is placed after the last *stable* message — everything
///    before the most recent exchange — because the conversation prefix grows
///    but never changes.
///
/// Anthropic allows at most four breakpoints per request, which this respects.
#[derive(Debug, Clone)]
pub struct CachePlan {
    /// The breakpoints, in request order.
    pub breakpoints: Vec<CacheBreakpoint>,
}

/// The maximum number of cache breakpoints one request may carry.
pub const MAX_BREAKPOINTS: usize = 4;

impl CachePlan {
    /// Plan breakpoints for a conversation of `message_count` messages.
    ///
    /// `long_session` selects the one-hour TTL for the stable prefix; pass true
    /// for an agent loop, false for a one-shot request where the extra write
    /// cost will never be repaid.
    pub fn for_conversation(
        message_count: usize,
        has_system: bool,
        has_tools: bool,
        long_session: bool,
    ) -> CachePlan {
        let stable_ttl = if long_session { CacheTtl::OneHour } else { CacheTtl::FiveMinutes };
        let mut breakpoints = Vec::new();

        if has_system {
            breakpoints.push(CacheBreakpoint::System(stable_ttl));
        }
        if has_tools {
            breakpoints.push(CacheBreakpoint::Tools(stable_ttl));
        }

        // Cache the conversation prefix, excluding the most recent exchange —
        // that part is what just changed, so a breakpoint after it would be
        // rewritten on the very next turn.
        //
        // Below four messages there is no stable prefix worth a write.
        if message_count >= 4 {
            let stable_end = message_count.saturating_sub(2);
            if breakpoints.len() < MAX_BREAKPOINTS {
                breakpoints.push(CacheBreakpoint::Message(
                    stable_end - 1,
                    CacheTtl::FiveMinutes,
                ));
            }
        }

        breakpoints.truncate(MAX_BREAKPOINTS);
        CachePlan { breakpoints }
    }

    /// The TTL to apply to the system prompt, if any.
    pub fn system_ttl(&self) -> Option<CacheTtl> {
        self.breakpoints.iter().find_map(|b| match b {
            CacheBreakpoint::System(ttl) => Some(*ttl),
            _ => None,
        })
    }

    /// The TTL to apply to the tool definitions, if any.
    pub fn tools_ttl(&self) -> Option<CacheTtl> {
        self.breakpoints.iter().find_map(|b| match b {
            CacheBreakpoint::Tools(ttl) => Some(*ttl),
            _ => None,
        })
    }

    /// The TTL to apply after the message at `index`, if any.
    pub fn message_ttl(&self, index: usize) -> Option<CacheTtl> {
        self.breakpoints.iter().find_map(|b| match b {
            CacheBreakpoint::Message(i, ttl) if *i == index => Some(*ttl),
            _ => None,
        })
    }

    /// Apply this plan to a request.
    pub fn apply(&self, request: &mut crate::provider::CompletionRequest) {
        request.cache_system = self.system_ttl();
        for (index, message) in request.messages.iter_mut().enumerate() {
            message.cache = self.message_ttl(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{CompletionRequest, Message};
    use crate::models::Model;

    #[test]
    fn ttls_use_the_documented_wire_values_and_multipliers() {
        assert_eq!(CacheTtl::FiveMinutes.as_str(), "5m");
        assert_eq!(CacheTtl::OneHour.as_str(), "1h");
        assert_eq!(CacheTtl::FiveMinutes.write_multiplier(), 1.25);
        assert_eq!(CacheTtl::OneHour.write_multiplier(), 2.0);
    }

    #[test]
    fn cache_control_serialises_as_the_api_expects() {
        assert_eq!(
            CacheTtl::OneHour.to_json(),
            serde_json::json!({ "type": "ephemeral", "ttl": "1h" })
        );
    }

    #[test]
    fn breakeven_reflects_the_write_premium() {
        // A 5m write costs 1.25 and each hit costs 0.1 instead of 1.0, so two
        // hits already repay it; the 1h write needs three.
        assert_eq!(CacheTtl::FiveMinutes.breakeven_hits(), 2);
        assert_eq!(CacheTtl::OneHour.breakeven_hits(), 3);
        assert!(
            CacheTtl::OneHour.breakeven_hits() > CacheTtl::FiveMinutes.breakeven_hits(),
            "the longer TTL costs more to write, so it needs more hits"
        );
    }

    #[test]
    fn a_long_session_caches_the_stable_prefix_for_an_hour() {
        let plan = CachePlan::for_conversation(10, true, true, true);
        assert_eq!(plan.system_ttl(), Some(CacheTtl::OneHour));
        assert_eq!(plan.tools_ttl(), Some(CacheTtl::OneHour));
    }

    #[test]
    fn a_one_shot_request_does_not_pay_the_one_hour_write_premium() {
        let plan = CachePlan::for_conversation(2, true, true, false);
        assert_eq!(
            plan.system_ttl(),
            Some(CacheTtl::FiveMinutes),
            "a request that will never be repeated must not pay 2x to cache"
        );
    }

    #[test]
    fn the_breakpoint_never_lands_on_the_turn_that_just_changed() {
        // With 10 messages, the last exchange is 8 and 9; a breakpoint there
        // would be rewritten on the very next turn.
        let plan = CachePlan::for_conversation(10, false, false, false);
        let message_breakpoints: Vec<usize> = plan
            .breakpoints
            .iter()
            .filter_map(|b| match b {
                CacheBreakpoint::Message(i, _) => Some(*i),
                _ => None,
            })
            .collect();

        assert_eq!(message_breakpoints, vec![7]);
        assert!(
            message_breakpoints.iter().all(|i| *i < 8),
            "the breakpoint must sit before the most recent exchange"
        );
    }

    #[test]
    fn a_short_conversation_gets_no_message_breakpoint() {
        // There is no stable prefix yet, so a write would never be repaid.
        for count in 0..4 {
            let plan = CachePlan::for_conversation(count, false, false, false);
            assert!(
                plan.breakpoints.is_empty(),
                "{count} messages should not get a breakpoint: {:?}",
                plan.breakpoints
            );
        }
    }

    #[test]
    fn the_api_breakpoint_limit_is_respected() {
        let plan = CachePlan::for_conversation(1000, true, true, true);
        assert!(
            plan.breakpoints.len() <= MAX_BREAKPOINTS,
            "the API rejects more than {MAX_BREAKPOINTS} breakpoints"
        );
    }

    #[test]
    fn applying_a_plan_marks_the_right_message() {
        let messages: Vec<Message> = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("turn {i}"))
                } else {
                    Message::assistant(format!("reply {i}"))
                }
            })
            .collect();
        let mut request = CompletionRequest::new(Model::Opus5, messages).system("system");

        let plan = CachePlan::for_conversation(10, true, false, true);
        plan.apply(&mut request);

        assert_eq!(request.cache_system, Some(CacheTtl::OneHour));
        assert_eq!(request.messages[7].cache, Some(CacheTtl::FiveMinutes));
        assert_eq!(request.messages[8].cache, None);
        assert_eq!(request.messages[9].cache, None, "the newest turn is never a breakpoint");
    }

    #[test]
    fn a_plan_without_a_system_prompt_does_not_mark_one() {
        let plan = CachePlan::for_conversation(10, false, false, true);
        assert_eq!(plan.system_ttl(), None);
        assert_eq!(plan.tools_ttl(), None);
    }
}
