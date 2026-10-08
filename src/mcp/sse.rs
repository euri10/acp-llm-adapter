//! Legacy MCP HTTP+SSE transport, owned by the SDK session without worker tasks.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Url};
use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use sse_reqwest_client::{EventSource, RequestBuilderExt as _, SseEvent, SseRetryConfig};

pub(super) const SETUP_LIMIT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("invalid legacy MCP SSE protocol: {0}")]
    Protocol(&'static str),
    #[error("invalid legacy MCP SSE URL")]
    Url(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("legacy MCP SSE HTTP request failed")]
    Http(#[from] reqwest::Error),
    #[error("legacy MCP SSE stream failed")]
    Stream(#[from] sse_reqwest_client::Error),
    #[error("legacy MCP SSE connection ended")]
    Connection(#[source] sse_reqwest_client::SseErrorEvent),
    #[error("invalid legacy MCP JSON-RPC message")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug)]
pub(super) struct SseTransport {
    client: Client,
    endpoint: Url,
    events: EventSource,
    failure: Option<Error>,
}

impl SseTransport {
    pub(super) async fn connect(
        url: &str,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<Self, Error> {
        let url = Url::parse(url).map_err(|error| Error::Url(Box::new(error)))?;
        validate_url(&url)?;
        let mut headers = headers.into_iter().collect::<HeaderMap>();
        for value in headers.values_mut() {
            // Arbitrary custom headers may carry credentials; never show them
            // in HTTP client's Debug diagnostics.
            value.set_sensitive(true);
        }
        let client = Client::builder()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(SETUP_LIMIT)
            .default_headers(headers)
            .build()?;
        let mut events = client
            .get(url.clone())
            .header(http::header::ACCEPT, "text/event-stream")
            .into_event_source_builder()
            // A reconnect creates a different legacy session and cannot replay
            // in-flight tool calls safely. Fail instead of silently reconnecting.
            .retry_config(SseRetryConfig::disabled())
            .fail_on_oversized_event(true)
            .build();
        // Legacy MCP requires the endpoint event before any JSON-RPC traffic:
        // https://modelcontextprotocol.io/specification/2024-11-05/basic/transports#http-with-sse
        let endpoint = loop {
            match events.next().await {
                Some(Ok(SseEvent::Open)) => {}
                Some(Ok(SseEvent::Message(event))) if event.event == "endpoint" => {
                    break message_endpoint(&url, &event.data)?;
                }
                Some(Ok(SseEvent::Message(_))) => {
                    return Err(Error::Protocol("expected endpoint event"));
                }
                Some(Ok(SseEvent::Error(error))) => return Err(Error::Connection(error)),
                Some(Err(error)) => return Err(error.into()),
                Some(Ok(SseEvent::Discarded(error))) => {
                    return Err(sse_reqwest_client::Error::PayloadTooLarge(error).into());
                }
                None => return Err(Error::Protocol("missing endpoint event")),
            }
        };
        Ok(Self {
            client,
            endpoint,
            events,
            failure: None,
        })
    }

    fn fail(&mut self, error: Error) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.failure = Some(error);
        self.events.close();
        None
    }
}

impl Transport<RoleClient> for SseTransport {
    type Error = Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Error>> + Send + 'static {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        async move {
            let response = client
                .post(endpoint)
                .header(http::header::CONTENT_TYPE, "application/json")
                .json(&item)
                .timeout(SETUP_LIMIT)
                .send()
                .await?
                .error_for_status()?;
            if !response.status().is_success() {
                return Err(Error::Protocol("POST did not accept the message"));
            }
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            match self.events.next().await {
                Some(Ok(SseEvent::Message(event))) if event.event == "message" => {
                    return match serde_json::from_str(&event.data) {
                        Ok(message) => Some(message),
                        Err(error) => self.fail(error.into()),
                    };
                }
                Some(Ok(SseEvent::Message(event))) if event.event == "endpoint" => {
                    return self.fail(Error::Protocol("duplicate endpoint event"));
                }
                // Ignore optional event types, but never discard malformed RPCs.
                Some(Ok(SseEvent::Open | SseEvent::Message(_))) => {}
                Some(Ok(SseEvent::Error(error))) => return self.fail(Error::Connection(error)),
                Some(Err(error)) => return self.fail(error.into()),
                Some(Ok(SseEvent::Discarded(error))) => {
                    return self.fail(sse_reqwest_client::Error::PayloadTooLarge(error).into());
                }
                None => return None,
            }
        }
    }

    async fn close(&mut self) -> Result<(), Error> {
        self.events.close();
        self.failure.take().map_or(Ok(()), Err)
    }
}

fn validate_url(url: &Url) -> Result<(), Error> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Protocol(
            "expected HTTP(S) URL without userinfo or fragment",
        ));
    }
    Ok(())
}

fn message_endpoint(base: &Url, endpoint: &str) -> Result<Url, Error> {
    if endpoint.trim().is_empty() {
        return Err(Error::Protocol("empty endpoint"));
    }
    let endpoint = base
        .join(endpoint)
        .map_err(|error| Error::Url(Box::new(error)))?;
    validate_url(&endpoint)?;
    // The server must not redirect configured credentials to another origin.
    if endpoint.origin() != base.origin() {
        return Err(Error::Protocol("endpoint must have the same origin"));
    }
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::{Url, message_endpoint, validate_url};

    #[test]
    fn legacy_endpoints_cannot_redirect_credentials() -> Result<(), Box<dyn std::error::Error>> {
        let base = Url::parse("https://example.test/sse?key=configured")?;
        assert_eq!(
            message_endpoint(&base, "/messages?session=1")?.as_str(),
            "https://example.test/messages?session=1"
        );
        assert!(message_endpoint(&base, "https://example.test/messages").is_ok());
        for endpoint in [
            "",
            " ",
            "http://example.test/messages",
            "https://other.test/messages",
            "//other.test/messages",
            "https://example.test:444/messages",
            "file:///secret",
            "https://user:secret@example.test/messages",
            "/messages#fragment",
        ] {
            assert!(
                message_endpoint(&base, endpoint).is_err(),
                "accepted {endpoint}"
            );
        }
        for url in [
            "file:///secret",
            "https://user:secret@example.test/sse",
            "https://example.test/sse#fragment",
        ] {
            assert!(validate_url(&Url::parse(url)?).is_err());
        }
        Ok(())
    }
}
