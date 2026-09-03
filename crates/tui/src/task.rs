//! Background tasks and typed one-shot result callbacks.
//!
//! Task errors are delivered as `Err(TaskError)` to the registered callback;
//! they are application data and do not shut down the runtime.

use std::any::Any;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc::UnboundedSender;

use crate::entity::EntityId;
use crate::subscription::RuntimeDelivery;

/// An error produced by background work.
pub struct TaskError(pub Box<dyn Error + Send + Sync>);
impl std::fmt::Debug for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl Error for TaskError {}

/// A type-erased completed task result. The runtime downcasts it to the
/// result type registered by `Context::spawn`.
pub struct TaskDelivery {
    pub(crate) id: TaskId,
    pub(crate) target: EntityId,
    pub(crate) result: Box<dyn Any + Send>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(u64);
impl TaskId {
    pub(crate) fn allocate() -> Self {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A handle for cancelling background work. Cancellation is best effort and
/// idempotent. Dropping a handle does not cancel the task.
#[derive(Clone)]
pub struct TaskHandle {
    active: Arc<AtomicBool>,
    cancel: Arc<dyn Fn() + Send + Sync>,
}
impl TaskHandle {
    pub fn new(cancel: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            cancel: Arc::new(cancel),
        }
    }
    pub fn cancel(&self) {
        if self.active.swap(false, Ordering::Release) {
            (self.cancel)();
        }
    }
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub(crate) fn with_cancel_cleanup(self, cleanup: impl Fn() + Send + Sync + 'static) -> Self {
        let active = Arc::clone(&self.active);
        let cancel = Arc::clone(&self.cancel);
        Self {
            active,
            cancel: Arc::new(move || {
                cancel();
                cleanup();
            }),
        }
    }
}
impl std::fmt::Debug for TaskHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskHandle")
            .field("active", &self.is_active())
            .finish_non_exhaustive()
    }
}

pub type DeliveryFuture = Pin<Box<dyn Future<Output = TaskDelivery> + Send>>;
pub type SubscriptionFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Executes tasks and stream workers outside runtime callbacks.
pub trait TaskExecutor: Send + Sync + 'static {
    fn spawn(&self, future: DeliveryFuture, sender: UnboundedSender<RuntimeDelivery>)
    -> TaskHandle;
    fn spawn_subscription(&self, future: SubscriptionFuture);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TokioExecutor;
impl TaskExecutor for TokioExecutor {
    fn spawn(
        &self,
        future: DeliveryFuture,
        sender: UnboundedSender<RuntimeDelivery>,
    ) -> TaskHandle {
        let active = Arc::new(AtomicBool::new(true));
        let task_active = Arc::clone(&active);
        let task = tokio::spawn(async move {
            let delivery = future.await;
            if task_active.swap(false, Ordering::AcqRel) {
                let _ = sender.send(RuntimeDelivery::Task(delivery));
            }
        });
        let abort = task.abort_handle();
        TaskHandle {
            active,
            cancel: Arc::new(move || abort.abort()),
        }
    }
    fn spawn_subscription(&self, future: SubscriptionFuture) {
        tokio::spawn(future);
    }
}
