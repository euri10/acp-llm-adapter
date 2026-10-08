//! Immutable limits for one tool-less selected-content Session.

use serde::{Deserialize, Serialize};

pub(crate) const META_KEY: &str = "io.github.euri10.louiselm.selectedContent";

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Limits {
    pub(crate) version: u32,
    pub(crate) input_bytes: usize,
    pub(crate) output_bytes: usize,
    pub(crate) max_tokens: u32,
    pub(crate) timeout_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::NewSessionRequest;

    #[test]
    fn rejects_malformed_or_amplifying_creation_contracts() {
        for value in [
            serde_json::json!(null),
            serde_json::json!({"version":2}),
            serde_json::json!({"version":1,"input_bytes":131_073,"output_bytes":128,"max_tokens":64,"timeout_ms":1000}),
            serde_json::json!({"version":1,"input_bytes":1024,"output_bytes":128,"max_tokens":64,"timeout_ms":30001}),
            serde_json::json!({"version":1,"input_bytes":1024,"output_bytes":128,"max_tokens":64,"timeout_ms":1000,"tools":true}),
        ] {
            let request = NewSessionRequest::new("/tmp")
                .meta(serde_json::Map::from_iter([(META_KEY.to_string(), value)]));
            assert!(Limits::from_request(&request).is_err());
        }
    }

    #[test]
    fn ordinary_sessions_are_unrestricted_and_helpers_refuse_extra_directories() {
        let mut request = NewSessionRequest::new("/tmp");
        assert!(matches!(Limits::from_request(&request), Ok(None)));
        request.meta = Some(serde_json::Map::from_iter([(
            META_KEY.to_string(),
            serde_json::json!({"version":1,"input_bytes":1024,"output_bytes":128,"max_tokens":64,"timeout_ms":1000}),
        )]));
        request.additional_directories.push("/etc".into());
        assert!(Limits::from_request(&request).is_err());
    }
}
