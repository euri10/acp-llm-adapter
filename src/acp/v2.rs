//! Draft ACP v2 agent for the `protocol-v2` probe (daa-acp-v2-37zc).
//!
//! `serve` routes each connection by its `initialize` request: v1 clients reach
//! the stable implementation in the parent module, v2 clients reach this one.
//! The v2 wire surface is translated here, at the edge, onto the same session
//! store and turn loop as v1.

use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, Client, ConnectTo, Responder, V2ConnectionTo};

use crate::{ADAPTER_NAME, ADAPTER_VERSION};

/// The v2 implementation handed to `Agent.protocol_router()`.
pub(crate) fn agent() -> impl ConnectTo<Client> {
    Agent.v2().name(ADAPTER_NAME).on_receive_request(
        async |_request: v2::InitializeRequest,
               responder: Responder<v2::InitializeResponse>,
               _connection: V2ConnectionTo<Client>| {
            responder.respond(initialize_response())
        },
        agent_client_protocol::on_receive_request!(),
    )
}

/// The v2 handshake. The router only selects this implementation for clients
/// that accept v2, so the answer is always v2. `session: {}` advertises the
/// whole baseline session surface and nothing optional: no MCP, no prompt
/// extensions and no selected-content extension.
pub(crate) fn initialize_response() -> v2::InitializeResponse {
    v2::InitializeResponse::new(
        ProtocolVersion::V2,
        v2::Implementation::new(ADAPTER_NAME, ADAPTER_VERSION),
    )
    .capabilities(v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()))
}
