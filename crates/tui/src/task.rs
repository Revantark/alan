//! Background tasks and command execution.
//!
//! Commands and tasks run external work (disk, web, computation) outside the
//! component callback and render paths. A task is a future whose output is a
//! typed application message delivered back to the spawning entity; errors
//! are preserved and never discarded — they abort the run as
//! [`RuntimeError::Task`](crate::RuntimeError) unless converted to a message
//! by the application first.
//!
//! Results identify their target by entity id captured at spawn time, so no
//! strong handle outlives the task and a removed component is not kept
//! alive: delivery to a removed entity is a safe no-op.

use std::error::Error;
use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc::UnboundedSender;

use crate::entity::EntityId;
use crate::subscription::RuntimeDelivery;

/// An error produced by a background task.
pub struct TaskError(pub Box<dyn Error + Send + Sync>);

impl std::fmt::Debug for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for TaskError {}

/// The result of a completed task, routed back to its spawning entity.
pub struct TaskDelivery<M> {
    /// Entity that spawned the task.
    pub target: EntityId,
    /// The task's outcome.
    pub result: Result<M, TaskError>,
}

/// A boxed task future producing a delivery routed to its target entity.
pub type DeliveryFuture<M> = Pin<Box<dyn Future<Output = TaskDelivery<M>> + Send>>;
pub type SubscriptionFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Runs background tasks, delivering results through `sender`.
///
/// The default implementation runs tasks on the surrounding tokio runtime.
pub trait TaskExecutor<M>: Send + Sync + 'static {
    /// Spawn a task; its delivery is sent to the runtime when complete.
    fn spawn(&self, future: DeliveryFuture<M>, sender: UnboundedSender<RuntimeDelivery<M>>);

    fn spawn_subscription(&self, future: SubscriptionFuture);
}

/// [`TaskExecutor`] backed by the tokio runtime.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioExecutor;

impl<M: Send + 'static> TaskExecutor<M> for TokioExecutor {
    fn spawn(&self, future: DeliveryFuture<M>, sender: UnboundedSender<RuntimeDelivery<M>>) {
        tokio::spawn(async move {
            let delivery = future.await;
            let _ = sender.send(RuntimeDelivery::Task(delivery));
        });
    }

    fn spawn_subscription(&self, future: SubscriptionFuture) {
        tokio::spawn(future);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokio_executor_delivers_result() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let executor = TokioExecutor;
        let target = crate::entity::EntityId::allocate();
        executor.spawn(
            Box::pin(async move {
                TaskDelivery {
                    target,
                    result: Ok("done".to_owned()),
                }
            }),
            sender,
        );
        let delivery: TaskDelivery<String> = match receiver.recv().await.unwrap() {
            RuntimeDelivery::Task(delivery) => delivery,
            RuntimeDelivery::Subscription(_) => panic!("unexpected subscription delivery"),
        };
        assert_eq!(delivery.result.unwrap(), "done");
        assert_eq!(delivery.target, target);
    }

    #[tokio::test]
    async fn tokio_executor_preserves_errors() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let executor = TokioExecutor;
        let target = crate::entity::EntityId::allocate();
        executor.spawn(
            Box::pin(async move {
                TaskDelivery {
                    target,
                    result: Err(TaskError(Box::new(std::io::Error::other("disk failure")))),
                }
            }),
            sender,
        );
        let delivery: TaskDelivery<String> = match receiver.recv().await.unwrap() {
            RuntimeDelivery::Task(delivery) => delivery,
            RuntimeDelivery::Subscription(_) => panic!("unexpected subscription delivery"),
        };
        assert!(delivery.result.is_err());
    }
}
