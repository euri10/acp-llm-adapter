//! Immutable limits for one tool-less selected-content Session.

use agent_client_protocol::schema::v1::NewSessionRequest;
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

impl Limits {
    pub(crate) fn from_request(
        request: &NewSessionRequest,
    ) -> Result<Option<Self>, agent_client_protocol::Error> {
        let Some(value) = request.meta.as_ref().and_then(|meta| meta.get(META_KEY)) else {
            return Ok(None);
        };
        let invalid = || {
            agent_client_protocol::Error::invalid_params()
                .data("invalid selected-content creation contract")
        };
        let limits: Self = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        if limits.version != 1
            || !(1..=131_072).contains(&limits.input_bytes)
            || !(1..=65_536).contains(&limits.output_bytes)
            || !(1..=8192).contains(&limits.max_tokens)
            || !(1..=30_000).contains(&limits.timeout_ms)
            || !request.mcp_servers.is_empty()
            || !request.additional_directories.is_empty()
        {
            return Err(invalid());
        }
        Ok(Some(limits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
