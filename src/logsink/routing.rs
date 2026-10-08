//! Session attribution for both directions of one ACP connection.

use std::collections::HashMap;

use serde_json::Value;

use super::Direction;

const METHOD_SESSION_NEW: &str = "session/new";
const METHOD_SESSION_LOAD: &str = "session/load";

/// What observing one frame revealed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Sniffed {
    pub(super) session_id: Option<String>,
    pub(super) established: Option<String>,
}

/// In-flight request ownership, separated by sending peer and JSON-RPC id type.
#[derive(Debug, Default)]
pub(super) struct SessionSniffer {
    // None marks session/new, whose response supplies its session id.
    client_requests: HashMap<String, Option<String>>,
    agent_requests: HashMap<String, Option<String>>,
}

impl SessionSniffer {
    pub(super) fn observe(&mut self, direction: Direction, frame: &Value) -> Sniffed {
        let (requests, responses) = match direction {
            Direction::ClientToAgent => (&mut self.client_requests, &mut self.agent_requests),
            Direction::AgentToClient => (&mut self.agent_requests, &mut self.client_requests),
            Direction::Internal => return Sniffed::default(),
        };
        let id = frame
            .get("id")
            .filter(|id| id.is_string() || id.is_number())
            .map(Value::to_string);
        if let Some(method) = frame.get("method").and_then(Value::as_str) {
            let explicit = frame
                .pointer("/params/sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(id) = id {
                if method == METHOD_SESSION_NEW && direction == Direction::ClientToAgent {
                    requests.insert(id, None);
                } else if explicit.is_some() {
                    requests.insert(id, explicit.clone());
                } else {
                    // A reused id for an unscoped request must not inherit old ownership.
                    requests.remove(&id);
                }
            }
            return Sniffed {
                established: if direction == Direction::ClientToAgent
                    && matches!(method, METHOD_SESSION_LOAD | "session/resume")
                {
                    explicit.clone()
                } else {
                    None
                },
                session_id: explicit,
            };
        }
        if frame.get("result").is_none() && frame.get("error").is_none() {
            return Sniffed::default();
        }
        match id.and_then(|id| responses.remove(&id)) {
            Some(Some(session_id)) => Sniffed {
                session_id: Some(session_id),
                established: None,
            },
            Some(None) => {
                let created = frame
                    .pointer("/result/sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Sniffed {
                    session_id: created.clone(),
                    established: created,
                }
            }
            None => Sniffed::default(),
        }
    }
}

#[cfg(test)]
mod tests;
