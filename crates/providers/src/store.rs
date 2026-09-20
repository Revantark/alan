use crate::{LocalModelEntry, ProviderError};

/// Persists local-model entries to disk.
///
/// `LocalProvider` holds a `Arc<dyn LocalModelStore>` and delegates all
/// file I/O here.  The concrete implementation lives in the `alan` crate
/// (which owns the `fs2` file-lock pattern and `JsonStore`).
#[async_trait::async_trait]
pub trait LocalModelStore: Send + Sync {
    /// Load entries from disk.  Missing file => empty vec.
    async fn load(&self) -> Result<Vec<LocalModelEntry>, ProviderError>;

    /// Overwrite the entire entry list on disk.
    async fn save(&self, entries: &[LocalModelEntry]) -> Result<(), ProviderError>;
}
