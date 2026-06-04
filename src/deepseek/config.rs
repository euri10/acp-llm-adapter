use std::env;

use super::DeepSeekError;

/// Configuration for the `DeepSeek` client.
///
/// Values can be provided explicitly with [`DeepSeekConfig::new`] or loaded
/// from the process environment with [`DeepSeekConfig::from_env`].
///
/// # Examples
///
/// ```rust
/// use deepseek_acp_adapter::deepseek::DeepSeekConfig;
///
/// let config = DeepSeekConfig::new(
///     "test-key",
///     "https://api.deepseek.com",
///     "deepseek-v4-pro",
/// );
///
/// assert_eq!(config.base_url(), "https://api.deepseek.com");
/// assert_eq!(config.model(), "deepseek-v4-pro");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepSeekConfig {
    api_key: String,
    base_url: String,
    model: String,
}

impl DeepSeekConfig {
    /// Default `DeepSeek` OpenAI-compatible base URL.
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
    /// # Errors
    ///
    /// Returns `MissingApiKey` when the API key is absent or empty.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use deepseek_acp_adapter::deepseek::DeepSeekConfig;
    ///
    /// let config = DeepSeekConfig::from_env()?;
    /// assert!(!config.model().is_empty());
    /// # Ok::<(), deepseek_acp_adapter::deepseek::DeepSeekError>(())
    /// ```
    pub fn from_env() -> Result<Self, DeepSeekError> {
        Self::from_environment(&SystemEnvironment)
    }

    pub(crate) fn from_environment(env: &impl Environment) -> Result<Self, DeepSeekError> {
        let api_key = env
            .var("DEEPSEEK_API_KEY")
            .ok_or(DeepSeekError::MissingApiKey)?;

        let api_key = api_key.trim().to_string();
        if api_key.is_empty() {
            return Err(DeepSeekError::MissingApiKey);
        }

        let base_url = env
            .var("DEEPSEEK_BASE_URL")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());

        let model = env
            .var("DEEPSEEK_MODEL")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| Self::DEFAULT_MODEL.to_string());

        Ok(Self {
            api_key,
            base_url,
            model,
        })
    }

    pub(crate) fn api_key(&self) -> &str {
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

pub(crate) trait Environment {
    fn var(&self, key: &str) -> Option<String>;
}

pub(crate) struct SystemEnvironment;

impl Environment for SystemEnvironment {
    fn var(&self, key: &str) -> Option<String> {
        env::var_os(key).and_then(|value| value.into_string().ok())
    }
}
