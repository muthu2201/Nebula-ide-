//! Token budgeting.
//!
//! The repo map has to fit inside a context budget, which means estimating how
//! many tokens a string will cost *before* sending it. Nebula does not bundle a
//! tokeniser: the vocabularies are provider-specific, they change between model
//! generations, and the 4.7+ Anthropic tokeniser produces roughly 30% more
//! tokens for the same text than its predecessor.
//!
//! So this module gives a deliberately conservative *estimate*, and the exact
//! count comes from the provider's own endpoint (`/v1/messages/count_tokens`)
//! when it matters. Over-estimating truncates the map slightly early;
//! under-estimating overflows the context window and fails the request, so the
//! estimator is tuned to err high.

/// Estimated tokens per character for source code.
///
/// Real-world code tokenises at roughly 3.0–3.5 characters per token — denser
/// than prose because identifiers, punctuation and indentation fragment. 3.0 is
/// used here to bias the estimate upwards.
const CHARS_PER_TOKEN: f64 = 3.0;

/// Estimate the number of tokens `text` will cost.
///
/// This is an upper-biased approximation, never an exact count.
pub fn estimate(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    // Count characters, not bytes: a multi-byte character is usually one token,
    // not four.
    let chars = text.chars().count() as f64;
    // Newlines almost always cost a token of their own and are undercounted by
    // a pure character ratio in heavily-indented code.
    let newlines = text.matches('\n').count() as f64;

    ((chars / CHARS_PER_TOKEN) + newlines * 0.5).ceil() as usize
}

/// Truncate `text` so its estimate fits within `budget` tokens.
///
/// Truncation happens at a line boundary — half a line of code in a context
/// window is worse than none, because a model will try to reason about the
/// fragment.
pub fn truncate_to_budget(text: &str, budget: usize) -> String {
    if estimate(text) <= budget {
        return text.to_string();
    }
    let mut out = String::new();
    for line in text.lines() {
        let candidate_len = out.len() + line.len() + 1;
        let candidate = &text[..candidate_len.min(text.len())];
        if estimate(candidate) > budget {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_costs_nothing() {
        assert_eq!(estimate(""), 0);
    }

    #[test]
    fn estimates_scale_with_length() {
        let short = estimate("fn main() {}");
        let long = estimate(&"fn main() {}\n".repeat(100));
        assert!(long > short * 50, "estimate must scale with input size");
    }

    #[test]
    fn the_estimate_errs_on_the_high_side() {
        // A rough sanity band: real code sits near 3-4 chars/token, so the
        // estimate should never fall below chars/4.
        let code = "pub fn compute(a: u32, b: u32) -> u32 {\n    a.wrapping_add(b)\n}\n";
        let estimated = estimate(code);
        let chars = code.chars().count();
        assert!(
            estimated >= chars / 4,
            "estimate {estimated} is below the chars/4 floor for {chars} chars"
        );
    }

    #[test]
    fn multibyte_characters_are_counted_as_characters_not_bytes() {
        let ascii = estimate(&"a".repeat(300));
        let unicode = estimate(&"é".repeat(300));
        assert_eq!(ascii, unicode, "a 2-byte char must not cost twice an ASCII char");
    }

    #[test]
    fn truncation_respects_the_budget() {
        let text = (0..500).map(|i| format!("line number {i}\n")).collect::<String>();
        let truncated = truncate_to_budget(&text, 100);
        assert!(estimate(&truncated) <= 100, "truncated text still exceeds the budget");
        assert!(!truncated.is_empty());
    }

    #[test]
    fn truncation_cuts_at_line_boundaries() {
        let text = "first line\nsecond line\nthird line\n";
        let truncated = truncate_to_budget(text, 5);
        assert!(
            truncated.is_empty() || truncated.ends_with('\n'),
            "truncated at a partial line: {truncated:?}"
        );
    }

    #[test]
    fn text_within_budget_is_returned_untouched() {
        let text = "short\n";
        assert_eq!(truncate_to_budget(text, 1000), text);
    }
}
