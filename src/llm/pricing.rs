use serde_json::Value;

use super::UsageData;

const ENV_PRICING: &str = "DEEPSEEK_PRICING";
const MICROS_PER_MILLION: u64 = 1_000_000;

#[derive(Debug, Clone, Copy)]
struct Pricing {
    cache_hit: u64,
    cache_miss: u64,
    output: u64,
}

/// Return the `DeepSeek` cost for one provider usage report in microdollars.
#[must_use]
pub fn deepseek_cost_micros(model: &str, usage: &UsageData) -> Option<u64> {
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
            deepseek_cost_micros("deepseek-v4-flash", &usage),
            Some(365_120)
        );
    }

    #[test]
    fn unknown_models_have_no_deepseek_price() {
        let usage = UsageData {
            input_tokens: 1,
            output_tokens: 1,
            context_length: 1,
            total_tokens: None,
            thought_tokens: None,
            cached_read_tokens: None,
            cached_write_tokens: None,
        };
        assert_eq!(deepseek_cost_micros("glm-5", &usage), None);
    }
}
