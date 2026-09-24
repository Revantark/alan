use crate::tool::ToolError;
use llm::{ToolCall, ToolKind};

pub fn parse(call: &ToolCall) -> Result<serde_json::Value, ToolError> {
    let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
        .map_err(|error| ToolError(format!("invalid tool arguments: {error}")))?;
    if !arguments.is_object() {
        return Err(ToolError("tool arguments must be a JSON object".into()));
    }
    Ok(arguments)
}

/// Parse the model-supplied `kind` field from a bash call's arguments.
pub(crate) fn tool_kind(arguments: &serde_json::Value) -> Result<ToolKind, ToolError> {
    arguments
        .get("kind")
        .map(|value| serde_json::from_value::<ToolKind>(value.clone()))
        .transpose()
        .map_err(|error| ToolError(format!("invalid kind argument: {error}")))?
        .ok_or_else(|| ToolError("missing argument: kind".into()))
}

/// Resolve the tool kind of a tool call.
///
/// Tools with statically known kinds are classified by name; `bash` carries
/// its `kind` inside its arguments. Anything unresolvable (unknown tool,
/// missing or invalid bash kind) is `ToolKind::Unknown` — fail-closed.
pub fn parse_kind(call: &ToolCall) -> ToolKind {
    match call.name.as_str() {
        "read" => ToolKind::Read,
        "write" | "edit" => ToolKind::Write,
        "bash" => parse(call)
            .and_then(|a| tool_kind(&a))
            .unwrap_or(ToolKind::Unknown),
        _ => ToolKind::Unknown,
    }
}

pub fn required_string(arguments: &serde_json::Value, name: &str) -> Result<String, ToolError> {
    arguments
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolError(format!("missing or invalid string argument: {name}")))
}

pub fn optional_u64(
    arguments: &serde_json::Value,
    name: &str,
    default: u64,
) -> Result<u64, ToolError> {
    arguments.get(name).map_or(Ok(default), |value| {
        value
            .as_u64()
            .ok_or_else(|| ToolError(format!("invalid non-negative integer argument: {name}")))
    })
}
