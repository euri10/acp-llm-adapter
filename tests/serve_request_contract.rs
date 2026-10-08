//! Provider request contracts exercised through the shipped ACP server.

mod acp_client;

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use acp_client::{Serve, Stopped};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::sync::mpsc;

struct Fixture {
    root: PathBuf,
    base_url: String,
    requests: mpsc::UnboundedReceiver<Value>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!("acp-requests-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        let (sender, requests) = mpsc::unbounded_channel();
        let router = Router::new()
            .route(
                "/models",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "application/json")],
                    r#"{"data":[{"id":"fixture-model"},{"id":"openai/gpt-oss-120b"},{"id":"openai/gpt-oss-20b"},{"id":"deepseek-v4-pro"},{"id":"glm-4.6"}]}"#,
                    )
                }),
            )
            .route("/chat/completions", post(capture_request))
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::error!(%error, "request fixture failed");
            }
        });
        Ok(Self {
            root,
            base_url,
            requests,
            server,
        })
    }

    async fn start(&self, selected_content: bool, model: &str) -> Result<Serve, Box<dyn Error>> {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_acp-llm-adapter"));
        command
            .args(["serve", "--backend", "groq"])
            .env("LLM_API_KEY", "fixture-key")
            .env("LLM_BASE_URL", &self.base_url)
            .env("LLM_MODEL", model)
            .env("XDG_STATE_HOME", &self.root)
            .env_remove("ACP_LOG")
            .env_remove("LLM_PRICING");
        let mut params = json!({"cwd": self.root, "mcpServers": []});
        if selected_content {
            params
                .as_object_mut()
                .ok_or("invalid fixture params")?
                .insert(
                    "_meta".into(),
                    json!({
                        "io.github.euri10.louiselm.selectedContent": {
                            "version":1, "input_bytes":1024, "output_bytes":1024,
                            "max_tokens":128, "timeout_ms":5000
                        }
                    }),
                );
        }
        Serve::start_with(command, params).await
    }

    async fn request(&mut self) -> Result<Value, Box<dyn Error>> {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await?
            .ok_or_else(|| "provider received no request".into())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn capture_request(
    State(sender): State<mpsc::UnboundedSender<Value>>,
    body: Bytes,
) -> Result<([(axum::http::HeaderName, &'static str); 1], String), StatusCode> {
    let request: Value = serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let deepseek = request.get("model") == Some(&json!("deepseek-v4-pro"));
    let invalid_history = deepseek
        && request
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| {
                messages
                    .iter()
                    .filter(|message| message.get("role") == Some(&json!("assistant")))
                    .any(|message| {
                        let expected = if message.get("tool_calls").is_some() {
                            "inspect the tool"
                        } else {
                            "answer the user"
                        };
                        message.get("reasoning_content") != Some(&json!(expected))
                            || message.get("content").is_none()
                    })
            });
    let last = request
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last());
    let tool = last.and_then(|message| message.get("content")) == Some(&json!("use tool"));
    sender
        .send(request)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if invalid_history {
        return Err(StatusCode::BAD_REQUEST);
    }
    let chunk = if tool {
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"fixture-tool",
            "function":{"name":"run_command","arguments":"{\"command\":\"printf fixture\"}"}}]},
            "finish_reason":"tool_calls"}]})
    } else {
        json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
    };
    let mut thoughts = String::new();
    if deepseek {
        let parts = if tool {
            ["inspect ", "the tool"]
        } else {
            ["answer ", "the user"]
        };
        for part in parts {
            thoughts.push_str("data: ");
            thoughts.push_str(
                &json!({"choices":[{"delta":{"reasoning_content":part},"finish_reason":null}]})
                    .to_string(),
            );
            thoughts.push_str("\n\n");
        }
    }
    Ok((
        [(CONTENT_TYPE, "text/event-stream")],
        format!("{thoughts}data: {chunk}\n\ndata: [DONE]\n\n"),
    ))
}

async fn prompt(serve: &mut Serve, session: &str, text: &str) -> Result<(), Box<dyn Error>> {
    let response = serve
        .request(
            "session/prompt",
            &json!({"sessionId":session,
        "prompt":[{"type":"text","text":text}]}),
        )
        .await?;
    assert_eq!(
        response.pointer("/result/stopReason"),
        Some(&json!("end_turn")),
        "{response}"
    );
    Ok(())
}

#[test_log::test(tokio::test)]
async fn idle_settings_storage_failure_preserves_acknowledged_options() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start(false, "openai/gpt-oss-120b").await?;
    let session = serve.session_id().to_owned();
    prompt(&mut serve, &session, "persist original settings").await?;
    fixture.request().await?;
    let blocked_path = fixture
        .root
        .join("acp-llm-adapter/sessions")
        .join(&session)
        .join("meta.json.tmp");
    std::fs::create_dir(&blocked_path)?;
    let response = serve
        .request(
            "session/set_mode",
            &json!({"sessionId":session,"modeId":"plan"}),
        )
        .await?;
    assert_eq!(
        response.pointer("/error/data"),
        Some(&json!("session storage failed"))
    );
    for (id, value) in [
        ("mode", "yolo"),
        ("model", "openai/gpt-oss-20b"),
        ("reasoning_effort", "high"),
        ("max_tokens", "4096"),
    ] {
        let response = set_config(&mut serve, &session, id, value).await?;
        assert_eq!(
            response.pointer("/error/data"),
            Some(&json!("session storage failed")),
            "{response}"
        );
    }
    std::fs::remove_dir(blocked_path)?;
    let response = set_config(&mut serve, &session, "max_tokens", "default").await?;
    let options = response
        .pointer("/result/configOptions")
        .and_then(Value::as_array)
        .ok_or("missing options")?;
    for (id, expected) in [
        ("mode", "ask"),
        ("model", "openai/gpt-oss-120b"),
        ("reasoning_effort", "default"),
        ("max_tokens", "default"),
    ] {
        let option = options
            .iter()
            .find(|option| option.get("id") == Some(&json!(id)))
            .ok_or("missing option")?;
        assert_eq!(option.get("currentValue"), Some(&json!(expected)));
    }
    prompt(&mut serve, &session, "usable after storage recovery").await?;
    let request = fixture.request().await?;
    assert_eq!(request.get("model"), Some(&json!("openai/gpt-oss-120b")));
    assert!(request.get("reasoning_effort").is_none());
    assert!(request.get("max_tokens").is_none());
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    Ok(())
}

#[test_log::test(tokio::test)]
async fn oversized_current_prompt_is_rejected_without_provider_work_and_session_recovers()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start(false, "fixture-model").await?;
    let session = serve.session_id().to_owned();
    let oversized = format!("USER-SENTINEL {}", "x".repeat(270_000));
    for subsequent in [false, true] {
        let response = serve
            .request(
                "session/prompt",
                &json!({
                    "sessionId":session,"prompt":[{"type":"text","text":oversized}]
                }),
            )
            .await?;
        assert_eq!(
            response.pointer("/error/code"),
            Some(&json!(-32602)),
            "oversized prompt was accepted (subsequent={subsequent})"
        );
        assert_eq!(
            response.pointer("/error/data"),
            Some(&json!("current prompt exceeds request size limit"))
        );
        assert!(
            fixture.requests.try_recv().is_err(),
            "rejected prompt reached provider"
        );
        let history_path = fixture
            .root
            .join("acp-llm-adapter/sessions")
            .join(&session)
            .join("history.jsonl");
        let history = match std::fs::read_to_string(history_path) {
            Ok(history) => history,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        assert!(
            !history.contains("USER-SENTINEL"),
            "rejected input changed persisted history"
        );
        let recovery = "RECOVERY-SENTINEL";
        prompt(&mut serve, &session, recovery).await?;
        let request = fixture.request().await?;
        assert_instruction(&request, false)?;
        let messages = request
            .get("messages")
            .and_then(Value::as_array)
            .ok_or("no messages")?;
        assert_eq!(
            messages.last().and_then(|message| message.get("content")),
            Some(&json!(recovery))
        );
        assert!(!request.to_string().contains("USER-SENTINEL"));
    }
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(3)).await?.success());
    Ok(())
}

fn assert_instruction(request: &Value, plan: bool) -> Result<(), Box<dyn Error>> {
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("missing messages")?;
    let systems = messages
        .iter()
        .filter(|message| message.get("role") == Some(&json!("system")))
        .collect::<Vec<_>>();
    assert_eq!(
        systems.len(),
        1,
        "one harness instruction per provider request"
    );
    let instruction = systems
        .first()
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .ok_or("missing instruction")?;
    assert!(instruction.contains("coding assistant"));
    assert!(instruction.contains("permission"));
    assert_eq!(instruction.contains("Plan mode"), plan);
    Ok(())
}

#[test_log::test(tokio::test)]
async fn ordinary_instruction_survives_tools_turns_and_restore_without_entering_history()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start(false, "fixture-model").await?;
    let session = serve.session_id().to_owned();
    prompt(&mut serve, &session, "use tool").await?;
    assert_instruction(&fixture.request().await?, false)?;
    assert_instruction(&fixture.request().await?, false)?;
    prompt(&mut serve, &session, "second turn").await?;
    assert_instruction(&fixture.request().await?, false)?;
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());

    for method in ["session/load", "session/resume"] {
        let mut serve = fixture.start(false, "fixture-model").await?;
        let response = serve
            .request(
                method,
                &json!({"sessionId": session, "cwd":fixture.root, "mcpServers":[]}),
            )
            .await?;
        assert!(response.get("error").is_none(), "{response}");
        let replay = serve.updates("agent_message_chunk");
        assert!(
            replay
                .iter()
                .all(|update| !update.to_string().contains("coding assistant"))
        );
        prompt(&mut serve, &session, "after restore").await?;
        assert_instruction(&fixture.request().await?, false)?;
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
    }
    let history = std::fs::read_to_string(
        fixture
            .root
            .join("acp-llm-adapter/sessions")
            .join(session)
            .join("history.jsonl"),
    )?;
    assert!(!history.contains("coding assistant"));
    assert!(!history.contains("\"system\""));
    Ok(())
}

#[test_log::test(tokio::test)]
async fn plan_composes_the_instruction_and_selected_content_keeps_its_own_contract()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    for selected_content in [false, true] {
        let mut serve = fixture.start(selected_content, "fixture-model").await?;
        let session = serve.session_id().to_owned();
        serve
            .request(
                "session/set_mode",
                &json!({"sessionId":session,"modeId":"plan"}),
            )
            .await?;
        prompt(&mut serve, &session, "make a plan").await?;
        let request = fixture.request().await?;
        if selected_content {
            assert_eq!(request.pointer("/messages/0/role"), Some(&json!("user")));
            assert!(request.get("tools").is_none());
        } else {
            assert_instruction(&request, true)?;
            let tools = request
                .get("tools")
                .and_then(Value::as_array)
                .ok_or("missing plan tools")?;
            assert!(
                tools
                    .iter()
                    .all(|tool| tool.pointer("/function/name") != Some(&json!("run_command")))
            );
        }
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn deepseek_reasoning_reaches_tool_followups_later_turns_and_restored_history()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start(false, "deepseek-v4-pro").await?;
    let session = serve.session_id().to_owned();
    prompt(&mut serve, &session, "use tool").await?;
    fixture.request().await?;
    let followup = fixture.request().await?;
    assert_eq!(
        followup.pointer("/messages/2/reasoning_content"),
        Some(&json!("inspect the tool"))
    );
    assert_eq!(followup.pointer("/messages/2/content"), Some(&json!("")));
    prompt(&mut serve, &session, "next turn").await?;
    let next = fixture.request().await?;
    assert_eq!(
        next.pointer("/messages/4/reasoning_content"),
        Some(&json!("answer the user"))
    );
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    for method in ["session/load", "session/resume"] {
        let mut serve = fixture.start(false, "deepseek-v4-pro").await?;
        let response = serve
            .request(
                method,
                &json!({"sessionId":session,"cwd":fixture.root,"mcpServers":[]}),
            )
            .await?;
        assert!(response.get("error").is_none(), "{response}");
        prompt(&mut serve, &session, "after restore").await?;
        let restored = fixture.request().await?;
        assert_eq!(
            restored.pointer("/messages/2/reasoning_content"),
            Some(&json!("inspect the tool"))
        );
        assert_eq!(
            restored.pointer("/messages/4/reasoning_content"),
            Some(&json!("answer the user"))
        );
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
    }
    Ok(())
}

async fn set_config(
    serve: &mut Serve,
    session: &str,
    field: &str,
    value: &str,
) -> Result<Value, Box<dyn Error>> {
    serve
        .request(
            "session/set_config_option",
            &json!({"sessionId": session, "configId": field, "value": value}),
        )
        .await
}

fn effort_option(response: &Value) -> Result<&Value, Box<dyn Error>> {
    response
        .pointer("/result/configOptions")
        .and_then(Value::as_array)
        .and_then(|options| {
            options
                .iter()
                .find(|option| option.get("id") == Some(&json!("reasoning_effort")))
        })
        .ok_or_else(|| format!("missing effort option: {response}").into())
}

#[test_log::test(tokio::test)]
async fn clear_command_resets_history_without_provider_work_and_preserves_settings()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start(false, "fixture-model").await?;
    let session = serve.session_id().to_owned();
    prompt(&mut serve, &session, "use tool").await?;
    fixture.request().await?;
    fixture.request().await?;
    for (field, value) in [
        ("model", "openai/gpt-oss-120b"),
        ("reasoning_effort", "high"),
        ("mode", "plan"),
        ("max_tokens", "4096"),
    ] {
        assert!(
            set_config(&mut serve, &session, field, value)
                .await?
                .get("error")
                .is_none()
        );
    }
    let history_path = fixture
        .root
        .join("acp-llm-adapter/sessions")
        .join(&session)
        .join("history.jsonl");
    for restore in [None, Some("session/load"), Some("session/resume")] {
        if let Some(method) = restore {
            let response = serve
                .request(
                    method,
                    &json!({"sessionId":session,"cwd":fixture.root,"mcpServers":[]}),
                )
                .await?;
            assert!(response.get("error").is_none(), "{response}");
            assert_eq!(
                effort_option(&response)?.get("currentValue"),
                Some(&json!("high"))
            );
            assert!(serve.updates("user_message_chunk").is_empty());
            assert!(serve.updates("agent_message_chunk").is_empty());
            prompt(&mut serve, &session, "fresh prompt").await?;
            let request = fixture.request().await?;
            assert_eq!(request.get("model"), Some(&json!("openai/gpt-oss-120b")));
            assert_eq!(request.get("reasoning_effort"), Some(&json!("high")));
            assert_eq!(request.get("max_tokens"), Some(&json!(4096)));
            assert_instruction(&request, true)?;
            assert_eq!(
                request
                    .get("messages")
                    .and_then(Value::as_array)
                    .map(Vec::len),
                Some(2)
            );
            assert_eq!(
                request.pointer("/messages/1/content"),
                Some(&json!("fresh prompt"))
            );
        }
        prompt(&mut serve, &session, "/clear").await?;
        assert!(
            matches!(
                fixture.requests.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "/clear called the provider"
        );
        assert_eq!(std::fs::read_to_string(&history_path)?, "");
        assert!(
            serve
                .updates("agent_message_chunk")
                .last()
                .is_some_and(|update| update.pointer("/content/text")
                    == Some(&json!("Conversation history cleared.")))
        );
        if restore.is_none() {
            prompt(&mut serve, &session, "fresh in memory").await?;
            let request = fixture.request().await?;
            assert_eq!(
                request
                    .get("messages")
                    .and_then(Value::as_array)
                    .map(Vec::len),
                Some(2)
            );
            assert_eq!(
                request.pointer("/messages/1/content"),
                Some(&json!("fresh in memory"))
            );
            prompt(&mut serve, &session, "/clear").await?;
            assert!(matches!(
                fixture.requests.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
        serve = fixture.start(false, "fixture-model").await?;
    }
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    Ok(())
}

#[test_log::test(tokio::test)]
async fn clear_command_is_exact_and_cannot_reset_selected_content_attempts()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut ordinary = fixture.start(false, "fixture-model").await?;
    let session = ordinary.session_id().to_owned();
    prompt(&mut ordinary, &session, "/clear this code").await?;
    assert_eq!(
        fixture.request().await?.pointer("/messages/1/content"),
        Some(&json!("/clear this code"))
    );
    let response = ordinary
        .request(
            "session/prompt",
            &json!({"sessionId":session,"prompt":[
                {"type":"text","text":"/clear"}, {"type":"text","text":"literal context"}
            ]}),
        )
        .await?;
    assert!(response.get("error").is_none());
    assert!(
        fixture
            .request()
            .await?
            .to_string()
            .contains("literal context")
    );
    ordinary.disconnect();
    assert!(ordinary.wait(Duration::from_secs(5)).await?.success());
    let mut selected = fixture.start(true, "fixture-model").await?;
    let session = selected.session_id().to_owned();
    prompt(&mut selected, &session, "/clear").await?;
    assert_eq!(
        fixture.request().await?.pointer("/messages/0/content"),
        Some(&json!("/clear"))
    );
    let denied = selected
        .request(
            "session/prompt",
            &json!({"sessionId":session,"prompt":[{"type":"text","text":"/clear"}]}),
        )
        .await?;
    assert_eq!(
        denied.pointer("/error/code").and_then(Value::as_i64),
        Some(-32600)
    );
    assert!(matches!(
        fixture.requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    selected.disconnect();
    assert!(selected.wait(Duration::from_secs(5)).await?.success());
    Ok(())
}

#[test_log::test(tokio::test)]
async fn clear_rejects_an_active_turn_without_discarding_its_history() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start(false, "fixture-model").await?;
    let session = serve.session_id().to_owned();
    let prompt_id = serve.start_prompt("use tool").await?;
    assert!(matches!(
        serve
            .pump_with_permission(Duration::from_secs(5), Some(prompt_id), || false, None)
            .await?,
        Stopped::Permission(_)
    ));
    fixture.request().await?;
    let denied = serve
        .request(
            "session/prompt",
            &json!({"sessionId":session,"prompt":[{"type":"text","text":"/clear"}]}),
        )
        .await?;
    assert_eq!(
        denied.pointer("/error/code").and_then(Value::as_i64),
        Some(-32600)
    );
    assert!(matches!(
        fixture.requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    let history = std::fs::read_to_string(
        fixture
            .root
            .join("acp-llm-adapter/sessions")
            .join(&session)
            .join("history.jsonl"),
    )?;
    assert!(history.contains("use tool"));
    serve
        .notify("session/cancel", &json!({"sessionId":session}))
        .await?;
    let Stopped::Response(response) = serve
        .pump(Duration::from_secs(5), Some(prompt_id), || false)
        .await?
    else {
        return Err("active turn did not cancel".into());
    };
    assert_eq!(
        response.pointer("/result/stopReason"),
        Some(&json!("cancelled"))
    );
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    Ok(())
}

#[test_log::test(tokio::test)]
async fn groq_explicit_high_is_sent_and_only_supported_efforts_are_advertised()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start(false, "openai/gpt-oss-120b").await?;
    let session = serve.session_id().to_owned();
    prompt(&mut serve, &session, "default effort").await?;
    assert!(fixture.request().await?.get("reasoning_effort").is_none());
    for effort in ["high", "low", "medium", "default"] {
        let response = set_config(&mut serve, &session, "reasoning_effort", effort).await?;
        let option = effort_option(&response)?;
        assert_eq!(option.get("currentValue"), Some(&json!(effort)));
        let values = option
            .get("options")
            .and_then(Value::as_array)
            .ok_or("missing effort values")?
            .iter()
            .filter_map(|value| value.get("value").and_then(Value::as_str))
            .collect::<Vec<_>>();
        prompt(&mut serve, &session, "configured effort").await?;
        let request = fixture.request().await?;
        if effort == "default" {
            assert!(request.get("reasoning_effort").is_none());
        } else {
            assert_eq!(request.get("reasoning_effort"), Some(&json!(effort)));
        }
        assert_eq!(values, ["default", "low", "medium", "high"]);
    }
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    Ok(())
}

#[test_log::test(tokio::test)]
async fn model_switch_resets_invalid_effort_and_invalid_updates_leave_state_unchanged()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start(false, "deepseek-v4-pro").await?;
    let session = serve.session_id().to_owned();
    let response = set_config(&mut serve, &session, "reasoning_effort", "max").await?;
    assert_eq!(
        effort_option(&response)?.get("currentValue"),
        Some(&json!("max"))
    );
    let response = set_config(&mut serve, &session, "model", "openai/gpt-oss-20b").await?;
    assert_eq!(
        effort_option(&response)?.get("currentValue"),
        Some(&json!("default"))
    );
    let invalid = set_config(&mut serve, &session, "reasoning_effort", "max").await?;
    assert_eq!(invalid.pointer("/error/code"), Some(&json!(-32602)));
    prompt(&mut serve, &session, "after rejected effort").await?;
    assert!(fixture.request().await?.get("reasoning_effort").is_none());
    for model in ["glm-4.6", "fixture-model"] {
        let response = set_config(&mut serve, &session, "model", model).await?;
        let values = effort_option(&response)?
            .get("options")
            .and_then(Value::as_array)
            .ok_or("missing options")?;
        assert_eq!(values.len(), 1);
        assert_eq!(
            values.first().and_then(|value| value.get("value")),
            Some(&json!("default"))
        );
        let invalid = set_config(&mut serve, &session, "reasoning_effort", "high").await?;
        assert_eq!(invalid.pointer("/error/code"), Some(&json!(-32602)));
    }
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    Ok(())
}

#[test_log::test(tokio::test)]
async fn restored_effort_is_validated_before_options_and_provider_requests()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let mut serve = fixture.start(false, "openai/gpt-oss-120b").await?;
    let session = serve.session_id().to_owned();
    prompt(&mut serve, &session, "persist the session").await?;
    fixture.request().await?;
    serve.disconnect();
    assert!(serve.wait(Duration::from_secs(5)).await?.success());
    let path = fixture
        .root
        .join("acp-llm-adapter/sessions")
        .join(&session)
        .join("meta.json");
    for method in ["session/load", "session/resume"] {
        for (persisted, expected) in [("max", "default"), ("high", "high")] {
            let mut meta: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            meta.as_object_mut()
                .ok_or("invalid fixture metadata")?
                .insert("reasoning_effort".into(), json!(persisted));
            std::fs::write(&path, serde_json::to_vec(&meta)?)?;
            let mut serve = fixture.start(false, "openai/gpt-oss-120b").await?;
            let response = serve
                .request(
                    method,
                    &json!({"sessionId":session,"cwd":fixture.root,"mcpServers":[]}),
                )
                .await?;
            assert_eq!(
                effort_option(&response)?.get("currentValue"),
                Some(&json!(expected))
            );
            prompt(&mut serve, &session, "restored effort").await?;
            let request = fixture.request().await?;
            if expected == "default" {
                assert!(request.get("reasoning_effort").is_none());
            } else {
                assert_eq!(request.get("reasoning_effort"), Some(&json!(expected)));
            }
            serve.disconnect();
            assert!(serve.wait(Duration::from_secs(5)).await?.success());
        }
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn idle_settings_survive_close_and_process_restart_without_a_followup_prompt()
-> Result<(), Box<dyn Error>> {
    for restore_method in ["session/load", "session/resume"] {
        let mut fixture = Fixture::new().await?;
        let mut serve = fixture.start(false, "openai/gpt-oss-120b").await?;
        let session = serve.session_id().to_owned();
        set_config(&mut serve, &session, "mode", "yolo").await?;
        prompt(&mut serve, &session, "save automatic approval").await?;
        fixture.request().await?;
        let response = if restore_method == "session/load" {
            serve
                .request(
                    "session/set_mode",
                    &json!({"sessionId":session,"modeId":"plan"}),
                )
                .await?
        } else {
            set_config(&mut serve, &session, "mode", "plan").await?
        };
        assert!(response.get("result").is_some(), "{response}");
        for (option, value) in [
            ("model", "openai/gpt-oss-20b"),
            ("reasoning_effort", "high"),
            ("max_tokens", "4096"),
        ] {
            let response = set_config(&mut serve, &session, option, value).await?;
            assert!(response.get("result").is_some(), "{response}");
        }
        let closed = serve
            .request("session/close", &json!({"sessionId":session}))
            .await?;
        assert!(closed.get("result").is_some(), "{closed}");
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());

        let mut serve = fixture.start(false, "openai/gpt-oss-120b").await?;
        let response = serve
            .request(
                restore_method,
                &json!({"sessionId":session,"cwd":fixture.root,"mcpServers":[]}),
            )
            .await?;
        assert_eq!(
            response.pointer("/result/modes/currentModeId"),
            Some(&json!("plan")),
            "{response}"
        );
        let options = response
            .pointer("/result/configOptions")
            .and_then(Value::as_array)
            .ok_or("missing restored settings")?;
        for (id, expected) in [
            ("model", "openai/gpt-oss-20b"),
            ("reasoning_effort", "high"),
            ("max_tokens", "4096"),
        ] {
            let option = options
                .iter()
                .find(|option| option.get("id") == Some(&json!(id)))
                .ok_or("missing restored option")?;
            assert_eq!(
                option.get("currentValue"),
                Some(&json!(expected)),
                "{restore_method}: {id}"
            );
        }
        prompt(&mut serve, &session, "use restored settings").await?;
        let request = fixture.request().await?;
        assert_eq!(request.get("model"), Some(&json!("openai/gpt-oss-20b")));
        assert_eq!(request.get("reasoning_effort"), Some(&json!("high")));
        assert_eq!(request.get("max_tokens"), Some(&json!(4096)));
        assert_instruction(&request, true)?;
        serve.disconnect();
        assert!(serve.wait(Duration::from_secs(5)).await?.success());
    }
    Ok(())
}
