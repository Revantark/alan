use llm::ImageUrl;

/// Builder for constructing an agent prompt request.
///
/// Created by [`Agent::prompt()`](super::Agent::prompt). Configure the
/// prompt with chained setter calls, then pass it to
/// [`Agent::ask()`](super::Agent::ask) to execute.
///
/// # Examples
///
/// ```ignore
/// // Streaming prompt
/// let stream = agent.ask(agent.prompt().content("hello").stream(true)).await?;
///
/// // Buffered prompt with images
/// let response = agent.ask(
///     agent.prompt().content("describe").images(vec![img])
/// ).await?.into_response().await?;
/// ```
pub struct PromptBuilder {
    pub(super) content: Option<String>,
    pub(super) images: Vec<ImageUrl>,
    pub(super) stream: bool,
}

impl PromptBuilder {
    pub(super) fn new() -> Self {
        Self {
            content: None,
            images: Vec::new(),
            stream: false,
        }
    }

    /// Set the text content of the prompt.
    pub fn content(mut self, text: impl Into<String>) -> Self {
        self.content = Some(text.into());
        self
    }

    /// Add one or more images to the prompt.
    pub fn images(mut self, images: impl IntoIterator<Item = ImageUrl>) -> Self {
        self.images.extend(images);
        self
    }

    /// Set the streaming mode.
    ///
    /// When `true`, the returned [`AgentStream`](super::AgentStream) emits
    /// incremental [`TextDelta`](super::AgentEvent::TextDelta) and
    /// [`ReasoningDelta`](super::AgentEvent::ReasoningDelta) events as the
    /// model generates them.
    ///
    /// When `false` (the default), only tool-call events and the final
    /// [`Finished`](super::AgentEvent::Finished) event are emitted.
    pub fn stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }
}
