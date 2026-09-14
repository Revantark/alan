/// An image attached to the next prompt via clipboard paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    pub name: String,
    pub mime_type: String,
    /// Raw base64-encoded image data (no `data:` prefix).
    pub base64_data: String,
}
