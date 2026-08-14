use crate::{Message, ToolSpec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    ApiKey(String),
    BearerToken(String),
    None,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

pub struct CompletionInput<'a> {
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub options: &'a RequestOptions,
}

pub struct LlmRequest<'a> {
    pub model_id: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub options: &'a RequestOptions,
    pub credential: Option<&'a Credential>,
}
