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
pub(crate) use registry::{
    AdapterToolRegistry, ToolContext, ToolExecution, ToolExecutor, ToolKind, ToolRegistry,
};
#[cfg(test)]
pub(crate) use registry::{EmptyToolRegistry, ToolEdit};
