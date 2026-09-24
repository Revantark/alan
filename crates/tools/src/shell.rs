use crate::args::{optional_u64, parse, required_string};
use crate::tool::{ToolError, ToolExecutor, ToolOutput};
use async_trait::async_trait;
use llm::{ToolCall, ToolDefinition};
use serde_json::json;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
const MAX_OUTPUT_BYTES: usize = 512 * 1000;

pub fn bash_definition() -> ToolDefinition {
    ToolDefinition {
        name: "bash".into(),
        description: "Execute any shell command. Always include a `kind` field in the arguments classifying this call's side effects".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["read", "write", "network", "unknown"],
                    "description": "Classify the bash command by its overall side effects. A command may contain multiple commands or pipelines. Apply these categories in strict precedence order: \"network\" if any part communicates with an external network resource; otherwise \"write\" if any part creates, modifies, or deletes persistent state; otherwise \"read\" if all parts only read or inspect state; otherwise \"unknown\". Always classify the complete command. Use unknown for commands which you can't classify"
                },
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Timeout in seconds",
                    "default": DEFAULT_TIMEOUT_SECONDS
                }
            },
            "required": ["command", "kind"]
        }),
    }
}

pub struct BashExecutor;

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[async_trait]
impl ToolExecutor for BashExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        let args = parse(call)?;
        let command = required_string(&args, "command")?;
        let timeout_seconds = optional_u64(&args, "timeout_seconds", DEFAULT_TIMEOUT_SECONDS)?;

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| ToolError(format!("failed to execute command: {error}")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError("failed to capture command stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError("failed to capture command stderr".into()))?;

        let result = tokio::time::timeout(Duration::from_secs(timeout_seconds), async {
            let (stdout, stderr) = tokio::join!(read_limited(stdout), read_limited(stderr));
            let stdout =
                stdout.map_err(|error| ToolError(format!("failed to read stdout: {error}")))?;
            let stderr =
                stderr.map_err(|error| ToolError(format!("failed to read stderr: {error}")))?;

            if stdout.len() > MAX_OUTPUT_BYTES || stderr.len() > MAX_OUTPUT_BYTES {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(ToolError(format!(
                    "command output exceeds {MAX_OUTPUT_BYTES} byte limit"
                )));
            }

            let status = child
                .wait()
                .await
                .map_err(|error| ToolError(format!("failed to wait for command: {error}")))?;
            Ok(CommandOutput {
                status,
                stdout,
                stderr,
            })
        })
        .await
        .map_err(|_| ToolError(format!("command timed out after {timeout_seconds}s")))??;

        let output = combine_output(&result.stdout, &result.stderr);

        if result.status.success() {
            Ok(ToolOutput::Text(output))
        } else {
            Err(ToolError(format!(
                "command failed (exit {}): {output}",
                result.status
            )))
        }
    }
}

fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);

    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.into_owned(),
        (true, false) => stderr.into_owned(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

async fn read_limited<R>(reader: R) -> Result<Vec<u8>, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take((MAX_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{parse_kind, tool_kind};
    use llm::ToolKind;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: arguments.into(),
            signature: None,
        }
    }

    #[test]
    fn parse_kind_maps_static_tools_by_name() {
        assert_eq!(parse_kind(&call("read", "{}")), ToolKind::Read);
        assert_eq!(parse_kind(&call("write", "{}")), ToolKind::Write);
        assert_eq!(parse_kind(&call("edit", "{}")), ToolKind::Write);
    }

    #[test]
    fn parse_kind_extracts_bash_kind_from_arguments() {
        for (kind, expected) in [
            ("read", ToolKind::Read),
            ("write", ToolKind::Write),
            ("network", ToolKind::Network),
            ("unknown", ToolKind::Unknown),
        ] {
            let arguments = format!(r#"{{"command": "ls", "kind": "{kind}"}}"#);
            assert_eq!(parse_kind(&call("bash", &arguments)), expected);
        }
    }

    #[test]
    fn parse_kind_fails_closed_for_bash() {
        assert_eq!(
            parse_kind(&call("bash", r#"{"command": "ls"}"#)),
            ToolKind::Unknown
        );
        assert_eq!(parse_kind(&call("bash", "not json")), ToolKind::Unknown);
        assert_eq!(
            parse_kind(&call("bash", r#"{"command": "ls", "kind": "bogus"}"#)),
            ToolKind::Unknown
        );
    }

    #[test]
    fn parse_kind_returns_unknown_for_unknown_tools() {
        assert_eq!(parse_kind(&call("mystery", "{}")), ToolKind::Unknown);
    }

    #[test]
    fn tool_kind_reports_missing_field() {
        let arguments = parse(&call("bash", "{}")).unwrap();
        assert!(tool_kind(&arguments).is_err());
    }
}
