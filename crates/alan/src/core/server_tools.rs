//! Mapping between provider server tools and the settings that toggle them,
//! shared by startup resolution, the profile apply flow, and profile capture.

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

/// Recover the `web_fetch`/`web_search` settings from the tools a model was
/// bound with, so a saved profile round-trips them.
pub fn web_flags(tools: &[llm::ServerTool]) -> (bool, bool) {
    let enabled = |kind: WebTool| {
        let id = WEB_TOOLS
            .iter()
            .find(|(_, k)| *k == kind)
            .map(|(id, _)| *id);

        tools.iter().any(|tool| Some(tool.kind.as_str()) == id)
    };

    (enabled(WebTool::Fetch), enabled(WebTool::Search))
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
    fn web_flags_round_trip_through_server_tools() {
        for (fetch, search) in [(false, false), (true, false), (false, true), (true, true)] {
            let available = [
                tool("openrouter:web_fetch"),
                tool("openrouter:web_search"),
                tool("some_other_provider:web_fetch"),
            ];
            let enabled = server_tools(&available, fetch, search);

            assert_eq!(web_flags(&enabled), (fetch, search));
        }
    }

    #[test]
    fn web_flags_ignores_unlisted_tools() {
        let bound = vec![llm::ServerTool {
            kind: "some_other_provider:web_fetch".to_owned(),
        }];

        assert_eq!(web_flags(&bound), (false, false));
    }
}
