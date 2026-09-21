use serde_json::Value;

use super::UsageData;

const ENV_PRICING: &str = "LLM_PRICING";
const MICROS_PER_MILLION: u64 = 1_000_000;

/// Per-million-token rates in microdollars.
///
/// Providers without a cached-prompt tier set `cache_hit` equal to
/// `cache_miss`, so the same arithmetic prices them at one flat input rate.
#[derive(Debug, Clone, Copy)]
struct Pricing {
    cache_hit: u64,
    cache_miss: u64,
    output: u64,
}

/// Return the cost for one provider usage report in microdollars.
///
/// Returns `None` for a model with no known published rates, which leaves the
/// `usage_update` without a cost rather than reporting an invented one.
#[must_use]
pub fn model_cost_micros(model: &str, usage: &UsageData) -> Option<u64> {
    let pricing = pricing_for(model)?;
    let cache_hit = usage.cached_read_tokens.unwrap_or(0);
    let cache_miss = usage
        .cached_write_tokens
        .unwrap_or_else(|| usage.input_tokens.saturating_sub(cache_hit));
    let uncached_input = usage.input_tokens.saturating_sub(cache_hit + cache_miss);
    let cost = cache_hit
        .saturating_mul(pricing.cache_hit)
        .saturating_add(cache_miss.saturating_mul(pricing.cache_miss))
        .saturating_add(uncached_input.saturating_mul(pricing.cache_miss))
        .saturating_add(usage.output_tokens.saturating_mul(pricing.output));
    Some(cost / MICROS_PER_MILLION)
}

fn pricing_for(model: &str) -> Option<Pricing> {
    let defaults = match model {
        "deepseek-v4-flash" => Pricing {
            cache_hit: 2_800,
            cache_miss: 140_000,
            output: 280_000,
        },
        "deepseek-v4-pro" => Pricing {
            cache_hit: 3_625,
            cache_miss: 435_000,
            output: 870_000,
        },
        // Groq does have a prompt-cache tier, at half the uncached input rate.
        // <https://console.groq.com/docs/model/openai/gpt-oss-120b>
        // <https://console.groq.com/docs/model/openai/gpt-oss-20b>
        // The 20b page rounds its cached rate to $0.037; the exact value is
        // half of $0.075, matching the ratio the 120b page states in full.
        "openai/gpt-oss-120b" => Pricing {
            cache_hit: 75_000,
            cache_miss: 150_000,
            output: 600_000,
        },
        "openai/gpt-oss-20b" => Pricing {
            cache_hit: 37_500,
            cache_miss: 75_000,
            output: 300_000,
        },
        _ => return None,
    };
    let Ok(raw) = std::env::var(ENV_PRICING) else {
        return Some(defaults);
    };
    let Ok(overrides) = serde_json::from_str::<Value>(&raw) else {
        return Some(defaults);
    };
    let Some(values) = overrides.get(model).and_then(Value::as_object) else {
        return Some(defaults);
    };
    Some(Pricing {
        cache_hit: price_override(values, "cache_hit", defaults.cache_hit),
        cache_miss: price_override(values, "cache_miss", defaults.cache_miss),
        output: price_override(values, "output", defaults.output),
    })
}

fn price_override(values: &serde_json::Map<String, Value>, key: &str, default: u64) -> u64 {
    values
        .get(key)
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| Some(value.to_string()))
        })
        .and_then(|value| parse_price_micros(&value))
        .unwrap_or(default)
}

fn parse_price_micros(value: &str) -> Option<u64> {
    let (whole, fraction) = value.split_once('.').map_or((value, ""), |parts| parts);
    if whole.is_empty() || !whole.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let whole = whole.parse::<u64>().ok()?.checked_mul(1_000_000)?;
    let fraction = fraction.chars().take(6).collect::<String>();
    if !fraction.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let fraction = format!("{fraction:0<6}").parse::<u64>().ok()?;
    whole.checked_add(fraction)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_deepseek_flash_cache_and_output_tokens() {
        let usage = UsageData {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            context_length: 1_000_000,
            total_tokens: None,
            thought_tokens: Some(100),
            cached_read_tokens: Some(400_000),
            cached_write_tokens: Some(600_000),
        };
        assert_eq!(
            model_cost_micros("deepseek-v4-flash", &usage),
            Some(365_120)
        );
    }

    #[test]
    fn unknown_models_have_no_price() {
        let usage = UsageData {
            input_tokens: 1,
            output_tokens: 1,
            context_length: 1,
            total_tokens: None,
            thought_tokens: None,
            cached_read_tokens: None,
            cached_write_tokens: None,
        };
        assert_eq!(model_cost_micros("glm-5", &usage), None);
    }

    /// Groq's prompt cache bills reads at half the uncached input rate, so a
    /// usage report that splits cache hits from misses must come out strictly
    /// cheaper than the same token count billed entirely uncached.
    #[test]
    fn groq_discounts_cached_input_tokens() {
        let uncached = UsageData {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            context_length: 131_072,
            total_tokens: None,
            thought_tokens: None,
            cached_read_tokens: None,
            cached_write_tokens: None,
        };
        let half_cached = UsageData {
            cached_read_tokens: Some(500_000),
            cached_write_tokens: Some(500_000),
            ..uncached
        };

        // 1M in at $0.15/M + 1M out at $0.60/M = $0.75.
        assert_eq!(
            model_cost_micros("openai/gpt-oss-120b", &uncached),
            Some(750_000)
        );
        // Half the input cached at $0.075/M: 0.5 * 0.15 + 0.5 * 0.075 + 0.60
        // = $0.7125.
        assert_eq!(
            model_cost_micros("openai/gpt-oss-120b", &half_cached),
            Some(712_500)
        );
    }

    #[test]
    fn groq_gpt_oss_20b_is_priced_below_the_120b() {
        let usage = UsageData {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            context_length: 131_072,
            total_tokens: None,
            thought_tokens: None,
            cached_read_tokens: None,
            cached_write_tokens: None,
        };
        // 1M in at $0.075/M + 1M out at $0.30/M = $0.375.
        assert_eq!(
            model_cost_micros("openai/gpt-oss-20b", &usage),
            Some(375_000)
        );
        assert!(
            model_cost_micros("openai/gpt-oss-20b", &usage)
                < model_cost_micros("openai/gpt-oss-120b", &usage)
        );
    }
}
