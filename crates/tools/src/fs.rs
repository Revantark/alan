use crate::args::{optional_u64, parse, required_string};
use crate::tool::{ToolError, ToolExecutor, ToolOutput};
use async_trait::async_trait;
use llm::{ToolCall, ToolDefinition};
use serde_json::json;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const MAX_FILE_SIZE: usize = 1024 * 1024;

pub fn file_read_definition() -> ToolDefinition {
    ToolDefinition {
        name: "read".into(),
        description: "Read a file, optionally limited to an inclusive line range".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                },
                "line_start": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "First line to read, inclusive"
                },
                "line_end": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Last line to read, inclusive"
                }
            },
            "required": ["path"]
        }),
    }
}

pub fn file_write_definition() -> ToolDefinition {
    ToolDefinition {
        name: "write".into(),
        description: "Write content to a file".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        }),
    }
}

pub fn file_edit_definition() -> ToolDefinition {
    ToolDefinition {
        name: "edit".into(),
        description: "Replace one unique text range in an existing file".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the existing file to edit"
                },
                "old_text": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Exact text to replace; must occur exactly once"
                },
                "new_text": {
                    "type": "string",
                    "description": "Text to write in its place"
                }
            },
            "required": ["path", "old_text", "new_text"]
        }),
    }
}

pub struct FileReadExecutor;

#[async_trait]
impl ToolExecutor for FileReadExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        let args = parse(call)?;
        let path = required_string(&args, "path")?;

        if let Some(mime_type) = image_mime_type(&path) {
            return read_image(&path, mime_type).await;
        }

        let line_start = optional_u64(&args, "line_start", 1)?;
        let line_end = args
            .get("line_end")
            .map(|value| {
                value.as_u64().ok_or_else(|| {
                    ToolError("invalid non-negative integer argument: line_end".into())
                })
            })
            .transpose()?;

        if line_start == 0 {
            return Err(ToolError("line_start must be at least 1".into()));
        }
        if line_end == Some(0) {
            return Err(ToolError("line_end must be at least 1".into()));
        }
        if let Some(line_end) = line_end
            && line_end < line_start
        {
            return Err(ToolError(
                "line_end must be greater than or equal to line_start".into(),
            ));
        }

        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| ToolError(format!("failed to read file {path}: {e}")))?;
        let mut reader = BufReader::new(file);
        let mut content = String::new();
        let mut line = String::new();
        let mut line_number = 1;

        loop {
            line.clear();
            let bytes_read = reader
                .read_line(&mut line)
                .await
                .map_err(|e| ToolError(format!("failed to read file {path}: {e}")))?;
            if bytes_read == 0 {
                break;
            }

            if line_number >= line_start {
                content.push_str(&line);
                if content.len() > MAX_FILE_SIZE {
                    return Err(ToolError(format!(
                        "file content exceeds {MAX_FILE_SIZE} byte limit"
                    )));
                }
            }

            if line_end == Some(line_number) {
                break;
            }
            line_number = line_number.saturating_add(1);
        }

        Ok(ToolOutput::Text(content))
    }
}

/// Return the MIME type for supported image extensions, or `None` for
/// non-image files.
fn image_mime_type(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// Read an image file and return it as base64-encoded `ToolOutput::Image`.
async fn read_image(path: &str, mime_type: &str) -> Result<ToolOutput, ToolError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| ToolError(format!("failed to read file {path}: {e}")))?;

    if bytes.len() > MAX_FILE_SIZE {
        return Err(ToolError(format!(
            "file content exceeds {MAX_FILE_SIZE} byte limit"
        )));
    }

    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(ToolOutput::Image {
        mime_type: mime_type.to_owned(),
        data,
    })
}

pub struct FileEditExecutor;

#[async_trait]
impl ToolExecutor for FileEditExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        let args = parse(call)?;
        let path = required_string(&args, "path")?;
        let old_text = required_string(&args, "old_text")?;
        let new_text = required_string(&args, "new_text")?;

        if old_text.is_empty() {
            return Err(ToolError("old_text must not be empty".into()));
        }

        let original = read_text_file(&path).await?;
        let matches: Vec<_> = original.match_indices(&old_text).collect();
        let (start, _) = match matches.as_slice() {
            [] => return Err(ToolError(format!("target not found: {old_text:?}"))),
            [matched] => *matched,
            _ => {
                let locations = matches
                    .iter()
                    .take(5)
                    .map(|(start, _)| line_number(&original, *start).to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ToolError(format!(
                    "target is ambiguous: matched {} times at lines {locations}",
                    matches.len()
                )));
            }
        };

        let end = start + old_text.len();
        let mut edited = String::with_capacity(original.len() - old_text.len() + new_text.len());
        edited.push_str(&original[..start]);
        edited.push_str(&new_text);
        edited.push_str(&original[end..]);

        if edited.len() > MAX_FILE_SIZE {
            return Err(ToolError(format!(
                "edited file exceeds {MAX_FILE_SIZE} byte limit"
            )));
        }

        tokio::fs::write(&path, edited)
            .await
            .map_err(|e| ToolError(format!("failed to edit file {}: {e}", path)))?;

        Ok(ToolOutput::Text(format!("File edited: {}", path)))
    }
}

async fn read_text_file(path: &str) -> Result<String, ToolError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ToolError(format!("failed to read file {path}: {e}")))?;
    let mut bytes = Vec::new();
    file.take((MAX_FILE_SIZE + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| ToolError(format!("failed to read file {path}: {e}")))?;

    if bytes.len() > MAX_FILE_SIZE {
        return Err(ToolError(format!(
            "file content exceeds {MAX_FILE_SIZE} byte limit"
        )));
    }

    String::from_utf8(bytes).map_err(|_| ToolError(format!("file is not valid UTF-8: {path}")))
}

fn line_number(content: &str, byte_offset: usize) -> usize {
    content[..byte_offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

pub struct FileWriteExecutor;

#[async_trait]
impl ToolExecutor for FileWriteExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        let args = parse(call)?;
        let path = required_string(&args, "path")?;
        let content = required_string(&args, "content")?;
        if content.len() > MAX_FILE_SIZE {
            return Err(ToolError(format!(
                "content exceeds {MAX_FILE_SIZE} byte limit"
            )));
        }

        if let Some(parent) = Path::new(&path)
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError(format!("failed to create parent directories: {e}")))?;
        }

        tokio::fs::write(&path, content)
            .await
            .map_err(|e| ToolError(format!("failed to write file {}: {e}", path)))?;

        Ok(ToolOutput::Text(format!("File written: {}", path)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tool_call(path: &Path, extra_args: &str) -> ToolCall {
        let arguments = if extra_args.is_empty() {
            format!(r#"{{"path":"{}"}}"#, path.display())
        } else {
            format!(r#"{{"path":"{}",{extra_args}}}"#, path.display())
        };

        ToolCall {
            id: "test-call".into(),
            name: "read".into(),
            arguments,
        }
    }

    fn temp_path() -> std::path::PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "alan-file-test-{}-{timestamp}-{id}.txt",
            std::process::id()
        ))
    }

    fn temp_path_with_ext(ext: &str) -> std::path::PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "alan-file-test-{}-{timestamp}-{id}.{ext}",
            std::process::id()
        ))
    }

    /// Minimal 1x1 white PNG (67 bytes).
    const MINIMAL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77,
        0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, // IDAT chunk
        0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xE2, 0x21,
        0xBC, 0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, // IEND chunk
        0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[tokio::test]
    async fn reads_selected_inclusive_line_range() {
        let path = temp_path();
        tokio::fs::write(&path, "one\ntwo\nthree\nfour\n")
            .await
            .unwrap();

        let call = tool_call(&path, r#""line_start":2,"line_end":3"#);
        let result = FileReadExecutor.execute(&call).await.unwrap();

        assert_eq!(result, ToolOutput::Text("two\nthree\n".into()));
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn reads_full_file_when_range_is_omitted() {
        let path = temp_path();
        tokio::fs::write(&path, "one\ntwo\n").await.unwrap();

        let result = FileReadExecutor
            .execute(&tool_call(&path, ""))
            .await
            .unwrap();

        assert_eq!(result, ToolOutput::Text("one\ntwo\n".into()));
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_line_range() {
        let path = temp_path();
        let call = tool_call(&path, r#""line_start":4,"line_end":2"#);

        let error = FileReadExecutor.execute(&call).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "tool execution failed: line_end must be greater than or equal to line_start"
        );
    }

    fn edit_call(path: &Path, old_text: &str, new_text: &str) -> ToolCall {
        ToolCall {
            id: "test-edit-call".into(),
            name: "edit".into(),
            arguments: serde_json::json!({
                "path": path,
                "old_text": old_text,
                "new_text": new_text
            })
            .to_string(),
        }
    }

    #[tokio::test]
    async fn replaces_unique_text() {
        let path = temp_path();
        tokio::fs::write(&path, "one\ntwo\nthree\n").await.unwrap();

        let call = edit_call(&path, "two", "TWO");
        let result = FileEditExecutor.execute(&call).await.unwrap();

        assert_eq!(
            result,
            ToolOutput::Text(format!("File edited: {}", path.display()))
        );
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "one\nTWO\nthree\n"
        );
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_repeated_text() {
        let path = temp_path();
        tokio::fs::write(&path, "same\nsame\n").await.unwrap();

        let call = edit_call(&path, "same", "changed");
        let error = FileEditExecutor.execute(&call).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "tool execution failed: target is ambiguous: matched 2 times at lines 1, 2"
        );
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "same\nsame\n"
        );
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn reads_png_as_image() {
        let path = temp_path_with_ext("png");
        tokio::fs::write(&path, MINIMAL_PNG).await.unwrap();

        let result = FileReadExecutor
            .execute(&tool_call(&path, ""))
            .await
            .unwrap();

        match &result {
            ToolOutput::Image { mime_type, data } => {
                assert_eq!(mime_type, "image/png");
                assert!(!data.is_empty());
                // Verify it's valid base64 that decodes back to the original bytes.
                use base64::Engine;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap();
                assert_eq!(decoded, MINIMAL_PNG);
            }
            other => panic!("expected Image, got {other:?}"),
        }
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn reads_jpg_extension_as_jpeg_image() {
        let path = temp_path_with_ext("jpg");
        tokio::fs::write(&path, b"not-really-jpg").await.unwrap();

        let result = FileReadExecutor
            .execute(&tool_call(&path, ""))
            .await
            .unwrap();

        match result {
            ToolOutput::Image { mime_type, .. } => assert_eq!(mime_type, "image/jpeg"),
            other => panic!("expected Image, got {other:?}"),
        }
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn reads_jpeg_extension_as_jpeg_image() {
        let path = temp_path_with_ext("jpeg");
        tokio::fs::write(&path, b"not-really-jpeg").await.unwrap();

        let result = FileReadExecutor
            .execute(&tool_call(&path, ""))
            .await
            .unwrap();

        match result {
            ToolOutput::Image { mime_type, .. } => assert_eq!(mime_type, "image/jpeg"),
            other => panic!("expected Image, got {other:?}"),
        }
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn reads_webp_as_image() {
        let path = temp_path_with_ext("webp");
        tokio::fs::write(&path, b"not-really-webp").await.unwrap();

        let result = FileReadExecutor
            .execute(&tool_call(&path, ""))
            .await
            .unwrap();

        match result {
            ToolOutput::Image { mime_type, .. } => assert_eq!(mime_type, "image/webp"),
            other => panic!("expected Image, got {other:?}"),
        }
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn reads_gif_as_image() {
        let path = temp_path_with_ext("gif");
        tokio::fs::write(&path, b"not-really-gif").await.unwrap();

        let result = FileReadExecutor
            .execute(&tool_call(&path, ""))
            .await
            .unwrap();

        match result {
            ToolOutput::Image { mime_type, .. } => assert_eq!(mime_type, "image/gif"),
            other => panic!("expected Image, got {other:?}"),
        }
        tokio::fs::remove_file(path).await.unwrap();
    }
}
