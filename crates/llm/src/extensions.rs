use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// Type-erased side-channel carrying provider-specific options on an
/// [`LlmRequest`](crate::LlmRequest).
#[derive(Clone, Default)]
pub struct Extensions {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Extensions {
    /// Store `value` under its type, replacing any previous value of the same
    /// type.
    pub fn insert<T: 'static + Send + Sync>(&mut self, value: T) {
        self.map.insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Borrow the value stored under type `T`, if any.
    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|value| value.as_ref().downcast_ref::<T>())
    }

    /// Copy every entry from `other`, overwriting same-keyed values. Use to
    /// layer per-request extensions (e.g. session id) over defaults held
    /// elsewhere (e.g. provider options from settings).
    pub fn merge(&mut self, other: &Extensions) {
        self.map
            .extend(other.map.iter().map(|(key, value)| (*key, value.clone())));
    }
}

/// Canonical session identity, set by the agent for every request. Codecs
/// translate it per provider: OpenRouter sends it as `session_id` and
/// `prompt_cache_key`, vanilla OpenAI-compatible APIs as `prompt_cache_key`.
pub struct SessionId(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_returns_typed_values() {
        let mut extensions = Extensions::default();
        extensions.insert(SessionId("abc".into()));

        assert_eq!(extensions.get::<SessionId>().unwrap().0, "abc");
        assert!(extensions.get::<String>().is_none());
    }

    #[test]
    fn replacing_a_key_overwrites_the_value() {
        let mut extensions = Extensions::default();
        extensions.insert(SessionId("one".into()));
        extensions.insert(SessionId("two".into()));

        assert_eq!(extensions.get::<SessionId>().unwrap().0, "two");
    }

    #[test]
    fn clones_share_values_without_observing_each_other() {
        let mut extensions = Extensions::default();
        extensions.insert(SessionId("abc".into()));
        let clone = extensions.clone();

        assert_eq!(clone.get::<SessionId>().unwrap().0, "abc");

        let mut detached = extensions.clone();
        detached.insert(SessionId("other".into()));
        assert_eq!(extensions.get::<SessionId>().unwrap().0, "abc");
        assert_eq!(detached.get::<SessionId>().unwrap().0, "other");
    }

    #[test]
    fn merge_layers_other_over_self() {
        struct Routing(Vec<String>);

        let mut base = Extensions::default();
        base.insert(SessionId("abc".into()));
        base.insert(Routing(vec!["a".into()]));

        let mut per_request = Extensions::default();
        per_request.insert(SessionId("xyz".into()));

        base.merge(&per_request);

        assert_eq!(base.get::<SessionId>().unwrap().0, "xyz");
        assert_eq!(base.get::<Routing>().unwrap().0, vec!["a"]);
    }
}
