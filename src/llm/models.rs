//! Static metadata for provider model IDs known to the adapter.

/// Known context-window sizes in tokens, keyed by model ID.
///
/// Some providers do not include `context_length`/`context_window` in their
/// usage payloads, so the adapter consults this table before sending an ACP
/// `usage_update`. Model IDs absent from the table are deliberately treated
/// as unknown; callers must not invent a window for them.
///
/// Values are sourced from provider documentation:
/// - `DeepSeek` V4: <https://api-docs.deepseek.com/quick_start/pricing>
/// - `GLM`-4.6: <https://docs.z.ai/guides/llm>
const KNOWN_CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("deepseek-v4-pro", 1_000_000),
    ("deepseek-v4-flash", 1_000_000),
    ("deepseek-v4-flash-vision-exp", 1_000_000),
    ("deepseek-chat", 4_096),
    ("glm-4.6", 131_072),
];

/// Return the context window size in tokens for a known model ID.
///
/// Returns `None` when the model is not listed in the known context-window
/// table for the adapter.
#[must_use]
pub fn context_window_for_model(model: &str) -> Option<u64> {
    KNOWN_CONTEXT_WINDOWS
        .iter()
        .find_map(|(known_model, window)| (*known_model == model).then_some(*window))
}

#[cfg(test)]
mod tests {
    use super::context_window_for_model;

    #[test]
    fn known_deepseek_v4_models_report_1m_window() {
        assert_eq!(context_window_for_model("deepseek-v4-pro"), Some(1_000_000));
        assert_eq!(
            context_window_for_model("deepseek-v4-flash"),
            Some(1_000_000)
        );
        assert_eq!(
            context_window_for_model("deepseek-v4-flash-vision-exp"),
            Some(1_000_000)
        );
    }

    #[test]
    fn legacy_deepseek_chat_reports_4096_window() {
        assert_eq!(context_window_for_model("deepseek-chat"), Some(4_096));
    }

    #[test]
    fn glm_46_reports_128k_window() {
        assert_eq!(context_window_for_model("glm-4.6"), Some(131_072));
    }

    #[test]
    fn unknown_models_return_none() {
        assert_eq!(context_window_for_model("mock-model"), None);
        assert_eq!(context_window_for_model("deepseek-v3"), None);
        assert_eq!(context_window_for_model(""), None);
    }
}
