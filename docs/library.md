# Library API

The crate also exposes a reusable `llm` module for request construction and
streaming response handling. Generate the API docs locally with:

```bash
cargo doc --no-deps
```

Typical library entry points:

- `llm::ChatMessage` for system, user, assistant, and tool-result messages
- `llm::ChatRequest` for model/tool request construction
- `llm::ToolDefinition` for JSON-schema tool advertisement
- `llm::StreamEvent` for normalized streamed output
- `llm::ChatClient` for HTTP-backed streaming requests

Minimal streaming example:

```rust,no_run
use acp_llm_adapter::llm::{ChatMessage, ChatRequest, ChatClient, LlmClient};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ChatClient::from_env()?;
    let request = ChatRequest::new(vec![ChatMessage::user("Summarize this repository")]);
    let mut stream = client.stream_chat(request, CancellationToken::new())?;

    while let Some(event) = stream.next().await {
        println!("{:?}", event?);
    }

    Ok(())
}
```
