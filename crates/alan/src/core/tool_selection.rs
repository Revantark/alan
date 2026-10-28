//! Selection of a provider's tools against the application settings.
//!
//! Providers advertise their full catalog; this module decides which of those
//! tools are enabled for a run, mapping each tool's [`Capability`] to the
//! setting that turns it on. Selection is app policy, so it lives here rather
//! than on the provider.

use providers::{Capability, ProviderId, ProviderRegistry, ProviderTool};

/// The tools of `provider_id` that the given settings enable.
///
/// Unknown providers yield an empty list; a tool without a capability is always
/// available.
pub fn select_tools(
    registry: &ProviderRegistry,
    provider_id: &ProviderId,
    web_fetch: bool,
    web_search: bool,
) -> Vec<ProviderTool> {
    registry.get(provider_id).map_or_else(Vec::new, |provider| {
        enable(provider.tools(), web_fetch, web_search)
    })
}

/// Filter a catalog down to the enabled tools.
fn enable(tools: &[ProviderTool], web_fetch: bool, web_search: bool) -> Vec<ProviderTool> {
    tools
        .iter()
        .filter(|tool| match tool.capability() {
            Some(Capability::WebFetch) => web_fetch,
            Some(Capability::WebSearch) => web_search,
            None => true,
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_on() -> ProviderTool {
        ProviderTool::server("bash", "run a command")
    }

    fn web(id: &str, capability: Capability) -> ProviderTool {
        ProviderTool::server(id, "web tool").with_capability(capability)
    }

    fn catalog() -> Vec<ProviderTool> {
        vec![
            always_on(),
            web("web_fetch", Capability::WebFetch),
            web("web_search", Capability::WebSearch),
        ]
    }

    fn ids(tools: &[ProviderTool]) -> Vec<&str> {
        tools.iter().map(ProviderTool::id).collect()
    }

    #[test]
    fn capability_less_tools_are_always_enabled() {
        assert_eq!(ids(&enable(&catalog(), false, false)), ["bash"]);
    }

    #[test]
    fn each_flag_enables_only_its_capability() {
        assert_eq!(ids(&enable(&catalog(), true, false)), ["bash", "web_fetch"]);
        assert_eq!(
            ids(&enable(&catalog(), false, true)),
            ["bash", "web_search"]
        );
        assert_eq!(
            ids(&enable(&catalog(), true, true)),
            ["bash", "web_fetch", "web_search"]
        );
    }

    #[test]
    fn empty_catalog_and_unknown_provider_yield_nothing() {
        assert!(enable(&[], true, true).is_empty());
        let registry = ProviderRegistry::new([]);
        assert!(select_tools(&registry, &ProviderId::new("missing"), true, true).is_empty());
    }

    #[test]
    fn selects_server_tools_from_a_real_provider() {
        let store: std::sync::Arc<dyn providers::CredentialStore> =
            std::sync::Arc::new(providers::InMemoryCredentialStore::new());
        let openrouter: std::sync::Arc<dyn providers::Provider> = std::sync::Arc::new(
            providers::OpenRouterProvider::from_store(store)
                .build()
                .unwrap(),
        );
        let registry = ProviderRegistry::new([openrouter]);

        let enabled = select_tools(&registry, &ProviderId::new("openrouter"), true, false);
        assert_eq!(ids(&enabled), ["openrouter:web_fetch"]);

        let disabled = select_tools(&registry, &ProviderId::new("openrouter"), false, false);
        assert!(disabled.is_empty());
    }

    #[test]
    fn selects_client_tools_from_deepseek() {
        let deepseek: std::sync::Arc<dyn providers::Provider> =
            std::sync::Arc::new(providers::DeepSeekProvider::builder("key").build().unwrap());
        let registry = ProviderRegistry::new([deepseek]);

        let enabled = select_tools(&registry, &ProviderId::new("deepseek"), false, true);
        assert_eq!(ids(&enabled), ["web_search"]);
    }
}
