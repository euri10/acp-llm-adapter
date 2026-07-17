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

    /// Name of the environment variable holding the LLM API key.
    pub const ENV_API_KEY: &str = "LLM_API_KEY";
    /// Name of the environment variable overriding the base URL.
    pub const ENV_BASE_URL: &str = "LLM_BASE_URL";
    /// Name of the environment variable overriding the model name.
    pub const ENV_MODEL: &str = "LLM_MODEL";

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

    /// Load config from `LLM_API_KEY`, `LLM_BASE_URL`, and `LLM_MODEL`.
    ///
    /// All three environment variables are provider-agnostic: the same var
    /// names work regardless of which `--backend` is selected. The backend
    /// only controls the *defaults* for base URL and model; these env vars
    /// override them.
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
        let api_key = get_env(Self::ENV_API_KEY).ok_or(ChatError::MissingApiKey)?;

        let api_key = api_key.trim().to_string();
        if api_key.is_empty() {
            return Err(ChatError::MissingApiKey);
        }

        let base_url = get_env(Self::ENV_BASE_URL)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());

        let model = get_env(Self::ENV_MODEL)
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
