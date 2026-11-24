use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::{ChatError, UsageData};

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

#[derive(Debug, Default)]
struct PricingOverride {
    cache_hit: Option<u64>,
    cache_miss: Option<u64>,
    output: Option<u64>,
}

/// Validate `LLM_PRICING` after tracing is initialized, warning on invalid input.
///
/// Invalid overrides are ignored; diagnostics never include their contents.
pub fn validate_pricing_overrides() {
    if let Err(reason) = pricing_overrides() {
        tracing::warn!(reason, "invalid LLM_PRICING; ignoring pricing overrides");
    }
}

/// Return the cost for one provider usage report in microdollars.
///
/// Returns `None` for a model with no known or overridden rates, which leaves
/// the `usage_update` without a cost rather than reporting an invented one.
///
/// # Errors
///
/// Returns [`ChatError::InvalidResponse`] for invalid token counters or a
/// cost that cannot be represented in microdollars.
pub fn model_cost_micros(model: &str, usage: &UsageData) -> Result<Option<u64>, ChatError> {
    usage.validated_total_tokens()?;
    pricing_for(model)
        .map(|pricing| cost_micros(pricing, usage))
        .transpose()
}

fn cost_micros(pricing: Pricing, usage: &UsageData) -> Result<u64, ChatError> {
    let cache_hit = usage.cached_read_tokens.unwrap_or(0);
    // Cache writes and uncached input have the same rate. Validation ensures
    // all billed counts together fit u64, so their weighted sum fits u128.
    let uncached_input = usage.input_tokens - cache_hit;
    let cost = u128::from(cache_hit) * u128::from(pricing.cache_hit)
        + u128::from(uncached_input) * u128::from(pricing.cache_miss)
        + u128::from(usage.output_tokens) * u128::from(pricing.output);
    u64::try_from(cost / u128::from(MICROS_PER_MILLION)).map_err(|_| {
        ChatError::InvalidResponse("usage cost exceeds the supported range".to_string())
    })
}

fn pricing_for(model: &str) -> Option<Pricing> {
    let defaults = builtin_pricing_at(model, SystemTime::now());
    // Invalid environment values were reported once during startup.
    let Ok(overrides) = pricing_overrides() else {
        return defaults;
    };
    let Some(values) = overrides.get(model) else {
        return defaults;
    };
    Some(Pricing {
        cache_hit: values.cache_hit.or(defaults.map(|price| price.cache_hit))?,
        cache_miss: values
            .cache_miss
            .or(defaults.map(|price| price.cache_miss))?,
        output: values.output.or(defaults.map(|price| price.output))?,
    })
}

fn builtin_pricing_at(model: &str, at: SystemTime) -> Option<Pricing> {
    let (price, deepseek) = match model {
        // USD/M tokens, off-peak; fetched 2026-11-24:
        // https://api-docs.deepseek.com/quick_start/pricing/
        "deepseek-flash" | "deepseek-v4-flash" | "deepseek-v4-flash-vision-exp" => (
            Pricing {
                cache_hit: 3_000,
                cache_miss: 150_000,
                output: 600_000,
            },
            true,
        ),
        // USD/M tokens, off-peak; fetched 2026-11-24:
        // https://api-docs.deepseek.com/quick_start/pricing/
        "deepseek-v4-pro" => (
            Pricing {
                cache_hit: 22_000,
                cache_miss: 660_000,
                output: 1_980_000,
            },
            true,
        ),
        // USD/M tokens; fetched 2026-11-24:
        // https://console.groq.com/docs/model/openai/gpt-oss-120b
        "openai/gpt-oss-120b" => (
            Pricing {
                cache_hit: 75_000,
                cache_miss: 150_000,
                output: 600_000,
            },
            false,
        ),
        // USD/M tokens; fetched 2026-11-24:
        // https://console.groq.com/docs/model/openai/gpt-oss-20b
        // The displayed $0.037 is rounded; Groq documents a 50% cache discount:
        // https://console.groq.com/docs/prompt-caching (fetched 2026-11-24).
        "openai/gpt-oss-20b" => (
            Pricing {
                cache_hit: 37_500,
                cache_miss: 75_000,
                output: 300_000,
            },
            false,
        ),
        _ => return None,
    };
    // Peak rates are twice off-peak: fetched 2026-11-24,
    // https://api-docs.deepseek.com/quick_start/pricing/
    if deepseek && is_deepseek_peak(at) {
        Some(Pricing {
            cache_hit: price.cache_hit * 2,
            cache_miss: price.cache_miss * 2,
            output: price.output * 2,
        })
    } else {
        Some(price)
    }
}

fn is_deepseek_peak(at: SystemTime) -> bool {
    let seconds = match at.duration_since(UNIX_EPOCH) {
        Ok(duration) => i128::from(duration.as_secs()),
        Err(error) => {
            let duration = error.duration();
            -i128::from(duration.as_secs()) - i128::from(duration.subsec_nanos() > 0)
        }
    };
    // Unix day zero was Thursday; Monday is weekday zero here. Windows fetched
    // 2026-11-24: https://api-docs.deepseek.com/quick_start/pricing/
    let weekday = (seconds.div_euclid(86_400) + 3).rem_euclid(7);
    let second_of_day = seconds.rem_euclid(86_400);
    weekday < 5
        && ((3_600..14_400).contains(&second_of_day) || (21_600..36_000).contains(&second_of_day))
}

fn pricing_overrides() -> Result<HashMap<String, PricingOverride>, &'static str> {
    let raw = match std::env::var(ENV_PRICING) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(HashMap::new()),
        Err(std::env::VarError::NotUnicode(_)) => return Err("value is not UTF-8"),
    };
    parse_pricing_overrides(&raw)
}

fn parse_pricing_overrides(raw: &str) -> Result<HashMap<String, PricingOverride>, &'static str> {
    let models: HashMap<String, serde_json::Map<String, Value>> =
        serde_json::from_str(raw).map_err(|_| "expected a JSON object of model rates")?;
    let mut result = HashMap::new();
    for (model, fields) in models {
        let mut values = PricingOverride::default();
        for (field, value) in fields {
            let price = match value {
                Value::String(price) => price,
                Value::Number(price) => price.to_string(),
                _ => return Err("rate must be a non-negative decimal"),
            };
            let price = parse_price_micros(&price).ok_or("invalid or unrepresentable rate")?;
            match field.as_str() {
                "cache_hit" => values.cache_hit = Some(price),
                "cache_miss" => values.cache_miss = Some(price),
                "output" => values.output = Some(price),
                _ => return Err("unknown rate field"),
            }
        }
        if builtin_pricing_at(&model, UNIX_EPOCH).is_none()
            && (values.cache_hit.is_none()
                || values.cache_miss.is_none()
                || values.output.is_none())
        {
            return Err("unknown model requires all three rates");
        }
        result.insert(model, values);
    }
    Ok(result)
}

fn parse_price_micros(value: &str) -> Option<u64> {
    let (mantissa, exponent) = match value.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
        None => (value, 0),
    };
    let (whole, fraction) = mantissa
        .split_once('.')
        .map_or((mantissa, ""), |parts| parts);
    if whole.is_empty()
        || !whole.chars().all(|character| character.is_ascii_digit())
        || !fraction.chars().all(|character| character.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}").parse::<u128>().ok()?;
    if digits == 0 {
        return Some(0);
    }
    let scale = 6 + i64::from(exponent) - i64::try_from(fraction.len()).ok()?;
    let factor = 10_u128.checked_pow(u32::try_from(scale.unsigned_abs()).ok()?)?;
    let micros = if scale >= 0 {
        digits.checked_mul(factor)?
    } else if digits % factor == 0 {
        digits / factor
    } else {
        return None;
    };
    u64::try_from(micros).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn override_rates_accept_exact_json_exponents_without_rounding() {
        let parsed = parse_pricing_overrides(
            r#"{"fixture":{"cache_hit":0.000001,"cache_miss":1e-6,"output":2}}"#,
        );
        let prices = parsed
            .as_ref()
            .ok()
            .and_then(|models| models.get("fixture"));
        assert_eq!(prices.and_then(|value| value.cache_hit), Some(1));
        assert_eq!(prices.and_then(|value| value.cache_miss), Some(1));
        assert!(
            parse_pricing_overrides(
                r#"{"fixture":{"cache_hit":0.0000001,"cache_miss":1,"output":2}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn deepseek_prices_follow_utc_weekday_windows() -> Result<(), ChatError> {
        let usage = UsageData {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            context_length: 1_000_000,
            total_tokens: None,
            thought_tokens: None,
            cached_read_tokens: Some(400_000),
            cached_write_tokens: Some(600_000),
        };
        // 1970-01-06 was Tuesday. Rates and windows: fetched 2026-11-24,
        // https://api-docs.deepseek.com/quick_start/pricing/
        let tuesday = UNIX_EPOCH + Duration::from_hours(120);
        let peak = builtin_pricing_at("deepseek-v4-pro", tuesday + Duration::from_hours(2))
            .ok_or_else(|| ChatError::InvalidResponse("missing Pro price".into()))?;
        let off_peak = builtin_pricing_at("deepseek-v4-pro", tuesday + Duration::from_hours(12))
            .ok_or_else(|| ChatError::InvalidResponse("missing Pro price".into()))?;
        assert_eq!(
            cost_micros(peak, &usage)?,
            2 * cost_micros(off_peak, &usage)?
        );
        for (day, hour, minute, peak) in [
            (5, 0, 59, false),
            (5, 1, 0, true),
            (5, 2, 0, true),
            (5, 4, 0, false),
            (5, 5, 59, false),
            (5, 6, 0, true),
            (5, 10, 0, false),
            (5, 12, 0, false),
            (2, 2, 0, false),
            (3, 2, 0, false),
            (4, 2, 0, true),
        ] {
            let at = UNIX_EPOCH + Duration::from_secs(day * 86_400 + hour * 3_600 + minute * 60);
            for (model, off_peak) in [
                ("deepseek-flash", 691_200),
                ("deepseek-v4-flash", 691_200),
                ("deepseek-v4-flash-vision-exp", 691_200),
                ("deepseek-v4-pro", 2_384_800),
            ] {
                let price = builtin_pricing_at(model, at).ok_or_else(|| {
                    ChatError::InvalidResponse(format!("missing price for {model}"))
                })?;
                assert_eq!(
                    cost_micros(price, &usage)?,
                    off_peak * if peak { 2 } else { 1 },
                    "{model} day={day} {hour}:{minute:02}"
                );
            }
        }
        Ok(())
    }

    #[test_log::test]
    fn wide_cost_intermediates_preserve_representable_results() -> Result<(), ChatError> {
        let usage = UsageData {
            input_tokens: u64::MAX,
            output_tokens: 0,
            context_length: 0,
            total_tokens: Some(u64::MAX),
            thought_tokens: None,
            cached_read_tokens: Some(u64::MAX / 2),
            cached_write_tokens: Some(u64::MAX - u64::MAX / 2),
        };
        assert_eq!(usage.validated_total_tokens()?, u64::MAX);
        let pricing = Pricing {
            cache_hit: MICROS_PER_MILLION,
            cache_miss: MICROS_PER_MILLION,
            output: MICROS_PER_MILLION,
        };
        assert_eq!(cost_micros(pricing, &usage)?, u64::MAX);
        let doubled = Pricing {
            cache_hit: 2 * MICROS_PER_MILLION,
            cache_miss: 2 * MICROS_PER_MILLION,
            ..pricing
        };
        assert!(matches!(
            cost_micros(doubled, &usage),
            Err(ChatError::InvalidResponse(_))
        ));
        Ok(())
    }

    #[test]
    fn current_deepseek_flash_has_a_published_price() -> Result<(), ChatError> {
        let usage = UsageData {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            context_length: 1_000_000,
            total_tokens: None,
            thought_tokens: None,
            cached_read_tokens: None,
            cached_write_tokens: None,
        };
        assert!(
            builtin_pricing_at("deepseek-flash", UNIX_EPOCH)
                .map(|price| cost_micros(price, &usage))
                .transpose()?
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn prices_deepseek_flash_cache_and_output_tokens() -> Result<(), ChatError> {
        let usage = UsageData {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            context_length: 1_000_000,
            total_tokens: None,
            thought_tokens: Some(100),
            cached_read_tokens: Some(400_000),
            cached_write_tokens: Some(600_000),
        };
        // Off-peak, fetched 2026-11-24: https://api-docs.deepseek.com/quick_start/pricing/
        let price = builtin_pricing_at("deepseek-v4-flash", UNIX_EPOCH)
            .ok_or_else(|| ChatError::InvalidResponse("missing Flash price".into()))?;
        assert_eq!(cost_micros(price, &usage)?, 691_200);
        Ok(())
    }

    #[test]
    fn unknown_models_have_no_price() -> Result<(), ChatError> {
        let usage = UsageData {
            input_tokens: 1,
            output_tokens: 1,
            context_length: 1,
            total_tokens: None,
            thought_tokens: None,
            cached_read_tokens: None,
            cached_write_tokens: None,
        };
        assert_eq!(model_cost_micros("glm-5", &usage)?, None);
        assert!(matches!(
            model_cost_micros(
                "glm-5",
                &UsageData {
                    output_tokens: u64::MAX,
                    ..usage
                }
            ),
            Err(ChatError::InvalidResponse(_))
        ));
        Ok(())
    }

    /// Groq's prompt cache bills reads at half the uncached input rate, so a
    /// usage report that splits cache hits from misses must come out strictly
    /// cheaper than the same token count billed entirely uncached.
    #[test]
    fn groq_discounts_cached_input_tokens() -> Result<(), ChatError> {
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
            model_cost_micros("openai/gpt-oss-120b", &uncached)?,
            Some(750_000)
        );
        // Half the input cached at $0.075/M: 0.5 * 0.15 + 0.5 * 0.075 + 0.60
        // = $0.7125.
        assert_eq!(
            model_cost_micros("openai/gpt-oss-120b", &half_cached)?,
            Some(712_500)
        );
        Ok(())
    }

    #[test]
    fn groq_gpt_oss_20b_is_priced_below_the_120b() -> Result<(), ChatError> {
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
            model_cost_micros("openai/gpt-oss-20b", &usage)?,
            Some(375_000)
        );
        assert!(
            model_cost_micros("openai/gpt-oss-20b", &usage)?
                < model_cost_micros("openai/gpt-oss-120b", &usage)?
        );
        Ok(())
    }
}
