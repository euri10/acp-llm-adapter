//! Built-in tool execution for the `acp-llm-adapter`.
//!
//! Modules:
//! - `registry`: `ToolContext`, `ToolRegistry`, registry impls, `ToolExecution`
//! - `execution`: tool definitions, execution functions, helpers, tests

pub(crate) mod execution;
mod filesystem;
mod registry;
mod search;

pub(crate) use execution::require_tool_permission;
#[cfg(test)]
pub(crate) use registry::EmptyToolRegistry;
pub(crate) use registry::{
    AdapterToolRegistry, ToolContext, ToolEdit, ToolExecution, ToolExecutor, ToolKind, ToolRegistry,
};
