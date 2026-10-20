//! Mapping between provider server tools and the settings that toggle them,
//! shared by startup resolution, the model picker, and the profile apply flow.

use providers::ServerToolInfo;

/// Which setting toggles a given provider tool id.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WebTool {
    Fetch,
    Search,
}

/// Every provider tool this app exposes a setting for, paired with that
/// setting. Add a row here when a provider offers a new web tool; ids not
/// listed are never enabled and are not captured into profiles.
const WEB_TOOLS: [(&str, WebTool); 2] = [
    ("openrouter:web_fetch", WebTool::Fetch),
    ("openrouter:web_search", WebTool::Search),
];

/// The web tools enabled by `web_fetch`/`web_search` out of `tools`.
pub fn server_tools(
    tools: &[ServerToolInfo],
    web_fetch: bool,
    web_search: bool,
) -> Vec<llm::ServerTool> {
    let is_on = |tool: WebTool| match tool {
        WebTool::Fetch => web_fetch,
        WebTool::Search => web_search,
    };

    tools
        .iter()
        .filter(|tool| {
            WEB_TOOLS
                .iter()
                .any(|(id, kind)| *id == tool.id && is_on(*kind))
        })
        .map(|tool| llm::ServerTool {
            kind: tool.id.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: &str) -> ServerToolInfo {
        ServerToolInfo {
            id: id.to_owned(),
            description: String::new(),
        }
    }

    #[test]
    fn includes_only_enabled_known_tools() {
        let tools = [tool("openrouter:web_fetch"), tool("openrouter:web_search")];
        let enabled = server_tools(&tools, true, false);
        assert_eq!(
            enabled,
            vec![llm::ServerTool {
                kind: "openrouter:web_fetch".to_owned()
            }]
        );
    }

    #[test]
    fn disabled_flags_and_unknown_tools_contribute_nothing() {
        let tools = [
            tool("openrouter:web_fetch"),
            tool("openrouter:web_search"),
            tool("openrouter:future_tool"),
        ];
        assert!(server_tools(&tools, false, false).is_empty());
    }

    #[test]
    fn empty_tool_list_yields_nothing() {
        assert!(server_tools(&[], true, true).is_empty());
    }

    #[test]
    fn unlisted_provider_tools_are_never_enabled() {
        let available = [
            tool("openrouter:web_fetch"),
            tool("some_other_provider:web_fetch"),
        ];
        let enabled = server_tools(&available, true, true);

        assert_eq!(
            enabled,
            vec![llm::ServerTool {
                kind: "openrouter:web_fetch".to_owned()
            }]
        );
    }
}
