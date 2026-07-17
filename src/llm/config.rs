use super::ChatError;

/// Configuration for the LLM client.
///
/// Values can be provided explicitly with [`ChatConfig::new`] or loaded
/// from the process environment with [`ChatConfig::from_env`].
///
/// # Examples
///
/// ```rust
/// use acp_llm_adapter::llm::ChatConfig;
///
/// let config = ChatConfig::new(
///     "test-key",
///     "https://api.deepseek.com",
///     "deepseek-v4-pro",
/// );
///
/// assert_eq!(config.base_url(), "https://api.deepseek.com");
/// assert_eq!(config.model(), "deepseek-v4-pro");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatConfig {
    api_key: String,
    base_url: String,
    model: String,
}

impl ChatConfig {
    /// Default `DeepSeek` base URL (used when `--backend deepseek`).
    pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
    /// Default model used by the adapter.
    pub const DEFAULT_MODEL: &str = "deepseek-v4-pro";

    /// Create a config from explicit values.
    #[must_use]
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    /// Load config from `DEEPSEEK_API_KEY`, `DEEPSEEK_BASE_URL`, and `DEEPSEEK_MODEL`.
    ///
    /// For the GLM backend, the API key is still read from `DEEPSEEK_API_KEY`;
    /// the base URL and model are overridden by the backend defaults.
    ///
    /// # Errors
    ///
    /// Returns `MissingApiKey` when the API key is absent or empty.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use acp_llm_adapter::llm::ChatConfig;
    ///
    /// let config = ChatConfig::from_env()?;
    /// assert!(!config.model().is_empty());
    /// # Ok::<(), acp_llm_adapter::llm::ChatError>(())
    /// ```
    pub fn from_env() -> Result<Self, ChatError> {
        Self::from_env_fn(|key| std::env::var_os(key).and_then(|value| value.into_string().ok()))
    }

    /// Load config from a caller-provided environment-variable lookup.
    ///
    /// This is the testable core of [`from_env`]; production code should use
    /// [`from_env`] directly.
    ///
    /// # Errors
    ///
    /// Same as [`from_env`].
    pub(crate) fn from_env_fn(
        mut get_env: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ChatError> {
        let api_key = get_env("DEEPSEEK_API_KEY").ok_or(ChatError::MissingApiKey)?;

        let api_key = api_key.trim().to_string();
        if api_key.is_empty() {
            return Err(ChatError::MissingApiKey);
        }

        let base_url = get_env("DEEPSEEK_BASE_URL")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());

        let model = get_env("DEEPSEEK_MODEL")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| Self::DEFAULT_MODEL.to_string());

        Ok(Self {
            api_key,
            base_url,
            model,
        })
    }

    /// Return the configured API key.
    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Return the configured base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Return the configured model name.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}
