use crate::tool::ToolError;
use llm::ToolCall;

pub fn parse(call: &ToolCall) -> Result<serde_json::Value, ToolError> {
    let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
        .map_err(|error| ToolError(format!("invalid tool arguments: {error}")))?;
    if !arguments.is_object() {
        return Err(ToolError("tool arguments must be a JSON object".into()));
    }
    Ok(arguments)
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
