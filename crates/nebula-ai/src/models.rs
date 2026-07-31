//! The model catalogue.
//!
//! Model identifiers, context windows, pricing and per-model quirks, kept in one
//! place so that a model deprecation is a single edit rather than a hunt through
//! the codebase.
//!
//! Pricing exists here for one reason: BYOK means the user pays the provider
//! directly, so Nebula's obligation is to show them what a request will cost
//! *before* they send it. It never marks anything up, because it never touches
//! the tokens.

use serde::{Deserialize, Serialize};

/// A model Nebula can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Model {
    /// Claude Opus 5 — flagship agentic coding.
    Opus5,
    /// Claude Sonnet 5 — the speed/intelligence balance.
    Sonnet5,
    /// Claude Fable 5 — the Mythos-class tier above Opus.
    Fable5,
    /// Claude Haiku 4.5 — fastest and cheapest.
    Haiku45,
}

/// Everything Nebula needs to know about a model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    /// The API identifier.
    pub id: &'static str,
    /// Name shown in the UI.
    pub display_name: &'static str,
    /// Context window in tokens.
    pub context_window: usize,
    /// Maximum output tokens in a normal request.
    pub max_output: usize,
    /// Maximum output tokens on the Message Batches API, which allows more via
    /// a beta header.
    pub max_output_batch: usize,
    /// US dollars per million input tokens.
    pub input_price_per_mtok: f64,
    /// US dollars per million output tokens.
    pub output_price_per_mtok: f64,
    /// Whether sampling parameters may be sent.
    ///
    /// Opus 4.7 and later reject `temperature`, `top_p` and `top_k` with a 400.
    /// Sending them is not a soft failure — the whole request fails — so the
    /// adapter consults this before building the body.
    pub accepts_sampling_params: bool,
    /// Whether the model supports the `effort` parameter.
    pub supports_effort: bool,
    /// Whether adaptive thinking is on by default.
    pub adaptive_thinking: bool,
}

impl Model {
    /// Every model in the catalogue.
    pub const ALL: &'static [Model] =
        &[Model::Fable5, Model::Opus5, Model::Sonnet5, Model::Haiku45];

    /// Static facts about this model.
    pub const fn info(&self) -> ModelInfo {
        match self {
            Model::Opus5 => ModelInfo {
                id: "claude-opus-5",
                display_name: "Claude Opus 5",
                context_window: 1_000_000,
                max_output: 128_000,
                max_output_batch: 300_000,
                input_price_per_mtok: 5.0,
                output_price_per_mtok: 25.0,
                accepts_sampling_params: false,
                supports_effort: true,
                adaptive_thinking: true,
            },
            Model::Sonnet5 => ModelInfo {
                id: "claude-sonnet-5",
                display_name: "Claude Sonnet 5",
                context_window: 1_000_000,
                max_output: 128_000,
                max_output_batch: 300_000,
                // Introductory pricing; rises to $3/$15 on 1 September 2026.
                input_price_per_mtok: 2.0,
                output_price_per_mtok: 10.0,
                accepts_sampling_params: false,
                supports_effort: true,
                adaptive_thinking: true,
            },
            Model::Fable5 => ModelInfo {
                id: "claude-fable-5",
                display_name: "Claude Fable 5",
                context_window: 1_000_000,
                max_output: 128_000,
                max_output_batch: 300_000,
                input_price_per_mtok: 10.0,
                output_price_per_mtok: 50.0,
                accepts_sampling_params: false,
                supports_effort: true,
                adaptive_thinking: true,
            },
            Model::Haiku45 => ModelInfo {
                id: "claude-haiku-4-5",
                display_name: "Claude Haiku 4.5",
                context_window: 200_000,
                max_output: 64_000,
                max_output_batch: 64_000,
                input_price_per_mtok: 1.0,
                output_price_per_mtok: 5.0,
                // Predates the 4.7 restriction, so sampling parameters are fine.
                accepts_sampling_params: true,
                supports_effort: false,
                adaptive_thinking: false,
            },
        }
    }

    /// The API identifier.
    pub const fn id(&self) -> &'static str {
        self.info().id
    }

    /// Look a model up by its API identifier.
    pub fn from_id(id: &str) -> Option<Model> {
        Self::ALL.iter().copied().find(|m| m.info().id == id)
    }

    /// The default model for interactive coding.
    pub const fn default_coding() -> Model {
        Model::Opus5
    }

    /// The default model for cheap background work — commit messages, titles,
    /// classification — where the flagship would be a waste of the user's money.
    pub const fn default_background() -> Model {
        Model::Haiku45
    }

    /// Estimated cost in US dollars for a given token count.
    ///
    /// Cache reads are billed at 0.1×, five-minute cache writes at 1.25× and
    /// one-hour writes at 2× the base input rate.
    pub fn estimate_cost(&self, usage: &crate::provider::Usage) -> f64 {
        let info = self.info();
        let input = info.input_price_per_mtok / 1_000_000.0;
        let output = info.output_price_per_mtok / 1_000_000.0;

        usage.input_tokens as f64 * input
            + usage.output_tokens as f64 * output
            + usage.cache_read_tokens as f64 * input * 0.1
            + usage.cache_write_5m_tokens as f64 * input * 1.25
            + usage.cache_write_1h_tokens as f64 * input * 2.0
    }
}

impl std::fmt::Display for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.info().display_name)
    }
}

impl Default for Model {
    fn default() -> Self {
        Model::default_coding()
    }
}

/// How much reasoning effort to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Minimal reasoning; fastest.
    Low,
    /// Balanced.
    Medium,
    /// Maximum reasoning. The API default.
    #[default]
    High,
}

impl Effort {
    /// The wire value.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Usage;

    #[test]
    fn every_model_round_trips_through_its_id() {
        for model in Model::ALL {
            assert_eq!(Model::from_id(model.info().id), Some(*model));
        }
        assert_eq!(Model::from_id("gpt-4"), None);
        assert_eq!(Model::from_id(""), None);
    }

    #[test]
    fn model_ids_are_the_documented_ones() {
        assert_eq!(Model::Opus5.id(), "claude-opus-5");
        assert_eq!(Model::Sonnet5.id(), "claude-sonnet-5");
        assert_eq!(Model::Fable5.id(), "claude-fable-5");
        assert_eq!(Model::Haiku45.id(), "claude-haiku-4-5");
    }

    #[test]
    fn the_five_series_models_reject_sampling_parameters() {
        // Sending temperature to these returns a 400 and fails the whole
        // request, so the adapter has to know not to.
        for model in [Model::Opus5, Model::Sonnet5, Model::Fable5] {
            assert!(
                !model.info().accepts_sampling_params,
                "{model} must not be sent temperature/top_p/top_k"
            );
        }
        assert!(Model::Haiku45.info().accepts_sampling_params);
    }

    #[test]
    fn the_five_series_models_have_a_one_million_token_context() {
        for model in [Model::Opus5, Model::Sonnet5, Model::Fable5] {
            assert_eq!(model.info().context_window, 1_000_000);
            assert_eq!(model.info().max_output, 128_000);
            assert_eq!(
                model.info().max_output_batch,
                300_000,
                "the batch API allows more output via a beta header"
            );
        }
    }

    #[test]
    fn pricing_matches_the_published_rates() {
        assert_eq!(Model::Opus5.info().input_price_per_mtok, 5.0);
        assert_eq!(Model::Opus5.info().output_price_per_mtok, 25.0);
        assert_eq!(Model::Fable5.info().input_price_per_mtok, 10.0);
        assert_eq!(Model::Haiku45.info().output_price_per_mtok, 5.0);
    }

    #[test]
    fn cost_estimation_uses_the_base_rates() {
        let usage = Usage { input_tokens: 1_000_000, output_tokens: 1_000_000, ..Usage::default() };
        // $5 in + $25 out.
        assert!((Model::Opus5.estimate_cost(&usage) - 30.0).abs() < 1e-9);
    }

    #[test]
    fn cache_reads_are_a_tenth_of_the_input_rate() {
        let uncached =
            Usage { input_tokens: 1_000_000, ..Usage::default() };
        let cached = Usage { cache_read_tokens: 1_000_000, ..Usage::default() };

        let full = Model::Opus5.estimate_cost(&uncached);
        let hit = Model::Opus5.estimate_cost(&cached);
        assert!(
            (hit - full * 0.1).abs() < 1e-9,
            "a cache hit costs {hit}, expected a tenth of {full}"
        );
    }

    #[test]
    fn cache_writes_cost_more_than_plain_input() {
        let plain = Usage { input_tokens: 1_000_000, ..Usage::default() };
        let write_5m = Usage { cache_write_5m_tokens: 1_000_000, ..Usage::default() };
        let write_1h = Usage { cache_write_1h_tokens: 1_000_000, ..Usage::default() };

        let base = Model::Opus5.estimate_cost(&plain);
        assert!((Model::Opus5.estimate_cost(&write_5m) - base * 1.25).abs() < 1e-9);
        assert!((Model::Opus5.estimate_cost(&write_1h) - base * 2.0).abs() < 1e-9);
    }

    #[test]
    fn an_empty_usage_costs_nothing() {
        assert_eq!(Model::Opus5.estimate_cost(&Usage::default()), 0.0);
    }

    #[test]
    fn the_background_default_is_cheaper_than_the_coding_default() {
        let background = Model::default_background().info();
        let coding = Model::default_coding().info();
        assert!(
            background.input_price_per_mtok < coding.input_price_per_mtok,
            "background work must not use the user's most expensive model"
        );
    }

    #[test]
    fn effort_defaults_to_high_as_the_api_does() {
        assert_eq!(Effort::default(), Effort::High);
        assert_eq!(Effort::default().as_str(), "high");
    }
}
