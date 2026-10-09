# Configuration

- Rust stable
- `LLM_API_KEY` (required for the DeepSeek, GLM and Groq backends)
- Optional: `LLM_BASE_URL` (overrides the provider's default base URL)
- Optional: `LLM_MODEL` (overrides the provider's default model)

Select a provider with `--backend deepseek|glm|groq|mock`. On both `serve` and `dev`, `--backend` is required. The `mock` backend requires no API key and is useful for local testing.

The reasoning-effort selector starts at **Provider default**, which omits the
parameter. Explicit choices are sent unchanged: Groq GPT-OSS offers Low, Medium
and High; known DeepSeek models offer Low, High and Max. Options follow the
provider contracts ([Groq](https://console.groq.com/docs/api-reference),
[DeepSeek](https://api-docs.deepseek.com/api/create-chat-completion/)). Other
models, including GLM-4.6, offer only Provider default until their effort contract
is supported. Invalid updates are rejected before changing state; switching or
restoring a model resets an unsupported stored effort to Provider default.

Every live backend is the same OpenAI-compatible client; the backend only chooses the defaults that `LLM_BASE_URL` and `LLM_MODEL` override:

| Backend | Default base URL | Default model |
| --- | --- | --- |
| `deepseek` | `https://api.deepseek.com` | `deepseek-v4-pro` |
| `glm` | `https://api.z.ai/api/paas/v4` | `glm-4.6` |
| `groq` | `https://api.groq.com/openai/v1` | `openai/gpt-oss-120b` |
