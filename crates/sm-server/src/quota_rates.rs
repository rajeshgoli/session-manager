//! Converting ledger tokens to percent of a weekly quota (sm#1662, ticket
//! #1675). Each model family has a fitted rate: percent of the weekly meter
//! per million weighted tokens. See
//! `specs/1662_analytics_redesign.html`, appendix B.1.

/// The day the rates below were fitted against meter readings.
pub const FITTED_AT: &str = "2026-09-29";

/// A Codex cloud PR review, as percent of a Codex week.
pub const CLOUD_REVIEW_PERCENT: f64 = 0.05;

/// Families in match order: the first substring found in the lower-cased
/// model name wins.
const FAMILIES: [&str; 8] = [
    "fable", "opus", "sonnet", "haiku", "astra", "sol", "terra", "luna",
];

/// Model names that are placeholders rather than models: priced at the
/// standard rate without a note.
const PLACEHOLDER_MODELS: [&str; 2] = ["unknown", "codex-auto-review"];

/// Token counts of one turn or a sum of turns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: i64,
    /// Includes reasoning tokens.
    pub output: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub cache_read: i64,
}

impl Tokens {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }

    pub fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.output += other.output;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.cache_read += other.cache_read;
    }
}

/// How a model name was priced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pricing {
    /// The fitted family, or `"other"` when the name matched none.
    pub family: &'static str,
    /// Percent of a week per million weighted tokens.
    pub rate: f64,
    /// True when the name matched no family and is not a known placeholder:
    /// the payload carries a note naming it.
    pub needs_note: bool,
}

/// Rate for `family` on `provider`; `None` for a family of the other
/// provider or an unknown one.
fn family_rate(provider: &str, family: &str) -> Option<f64> {
    match (provider, family) {
        ("claude", "fable") => Some(0.648),
        ("claude", "opus") => Some(0.167),
        ("claude", "sonnet") => Some(0.100),
        ("claude", "haiku") => Some(0.033),
        ("codex", "astra") => Some(0.683),
        ("codex", "sol") => Some(0.228),
        ("codex", "terra") => Some(0.137),
        ("codex", "luna") => Some(0.030),
        _ => None,
    }
}

/// The provider's standard model, which prices any name with no family.
pub fn standard_family(provider: &str) -> &'static str {
    if provider == "codex" {
        "sol"
    } else {
        "opus"
    }
}

pub fn family_label(family: &str) -> &'static str {
    match family {
        "fable" => "Fable",
        "opus" => "Opus",
        "sonnet" => "Sonnet",
        "haiku" => "Haiku",
        "astra" => "Astra",
        "sol" => "Sol",
        "terra" => "Terra",
        "luna" => "Luna",
        "review" => "Cloud reviews",
        _ => "Other",
    }
}

pub fn pricing(provider: &str, model: &str) -> Pricing {
    let lower = model.to_ascii_lowercase();
    let matched = FAMILIES
        .iter()
        .find(|family| lower.contains(*family))
        .and_then(|family| family_rate(provider, family).map(|rate| (*family, rate)));
    match matched {
        Some((family, rate)) => Pricing {
            family,
            rate,
            needs_note: false,
        },
        None => Pricing {
            family: "other",
            rate: family_rate(provider, standard_family(provider)).unwrap_or(0.0),
            needs_note: !PLACEHOLDER_MODELS.contains(&lower.as_str()),
        },
    }
}

/// Weighted tokens (millions are applied by `fitted_percent`).
pub fn weighted_tokens(provider: &str, tokens: &Tokens) -> f64 {
    let input = tokens.input as f64;
    let output = tokens.output as f64;
    let cache_read = tokens.cache_read as f64;
    if provider == "codex" {
        input + 8.0 * output + 0.10 * cache_read
    } else {
        input
            + 8.0 * output
            + 1.25 * tokens.cache_write_5m as f64
            + 2.0 * tokens.cache_write_1h as f64
            + 0.02 * cache_read
    }
}

/// Fitted percent of a week for `tokens` of `model`, before scaling to the
/// meter.
pub fn fitted_percent(provider: &str, model: &str, tokens: &Tokens) -> f64 {
    weighted_tokens(provider, tokens) / 1_000_000.0 * pricing(provider, model).rate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    #[test]
    fn weighted_tokens_follow_each_providers_formula() {
        let tokens = Tokens {
            input: 1_000,
            output: 100,
            cache_write_5m: 400,
            cache_write_1h: 200,
            cache_read: 10_000,
        };
        // 1000 + 800 + 500 + 400 + 200
        assert!(close(weighted_tokens("claude", &tokens), 2_900.0));
        // 1000 + 800 + 1000; Codex has no cache writes.
        assert!(close(weighted_tokens("codex", &tokens), 2_800.0));
    }

    #[test]
    fn families_price_at_their_fitted_rates() {
        let cases = [
            ("claude", "claude-fable-5-1", "fable", 0.648),
            ("claude", "claude-opus-5-5", "opus", 0.167),
            ("claude", "claude-opus-4-8", "opus", 0.167),
            ("claude", "claude-sonnet-5", "sonnet", 0.100),
            ("claude", "claude-haiku-4-5-20251001", "haiku", 0.033),
            ("codex", "gpt-6-astra", "astra", 0.683),
            ("codex", "gpt-5.6-sol", "sol", 0.228),
            ("codex", "gpt-6-sol", "sol", 0.228),
            ("codex", "gpt-5.6-terra", "terra", 0.137),
            ("codex", "gpt-5.6-luna", "luna", 0.030),
        ];
        for (provider, model, family, rate) in cases {
            let priced = pricing(provider, model);
            assert_eq!(priced.family, family, "{model}");
            assert!(close(priced.rate, rate), "{model}");
            assert!(!priced.needs_note, "{model}");
        }
        // The 1787 spec author's week: 0.36M output, 0.9M 1-hour writes,
        // 67M reads is about 6.0M weighted, 3.9% fitted.
        let author = Tokens {
            output: 360_000,
            cache_write_1h: 900_000,
            cache_read: 67_000_000,
            ..Tokens::default()
        };
        let fitted = fitted_percent("claude", "claude-fable-5-1", &author);
        assert!((fitted - 3.9).abs() < 0.05, "{fitted}");
    }

    #[test]
    fn unknown_models_fall_back_to_the_standard_rate_and_ask_for_a_note() {
        let new_model = pricing("codex", "gpt-7-x");
        assert_eq!(new_model.family, "other");
        assert!(close(new_model.rate, 0.228));
        assert!(new_model.needs_note);

        let new_claude = pricing("claude", "claude-mystery-6");
        assert_eq!(new_claude.family, "other");
        assert!(close(new_claude.rate, 0.167));
        assert!(new_claude.needs_note);

        for placeholder in ["unknown", "codex-auto-review"] {
            let priced = pricing("codex", placeholder);
            assert_eq!(priced.family, "other");
            assert!(close(priced.rate, 0.228));
            assert!(!priced.needs_note, "{placeholder}");
        }
        // A family of the other provider does not price across meters.
        assert_eq!(pricing("claude", "gpt-6-astra").family, "other");
    }
}
