//! Components, actions, and read-only rendering.
//!
//! Actions are synchronous semantic commands routed through the overlay,
//! focused entity, and parent hierarchy. Targeted coordination uses
//! [`Context::dispatch`](crate::context::Context::dispatch) and
//! [`Context::update`](crate::context::Context::update). State invalidation is
//! expressed with `notify`/`observe`; meaningful occurrences use typed
//! `emit`/`subscribe`; external streams use `subscribe_stream`.
//!
//! Rendering is read-only and performs no I/O or mutation.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::context::Context;
use crate::entity::{Entity, EntityId, EntityStore};

/// Read-only capabilities available during render.
pub struct RenderContext<'a, A: 'static> {
    pub(crate) store: &'a EntityStore<A>,
    /// The entity that currently holds focus, if any.
    pub(crate) focused: Option<EntityId>,
    /// The entity this context renders.
    pub(crate) entity: Option<EntityId>,
}

impl<'a, A> RenderContext<'a, A> {
    pub(crate) fn new(
        store: &'a EntityStore<A>,
        focused: Option<EntityId>,
        entity: Option<EntityId>,
    ) -> Self {
        Self {
            store,
            focused,
            entity,
        }
    }

    /// Whether the entity being rendered currently holds focus.
    pub fn is_focused(&self) -> bool {
        self.entity.is_some() && self.focused == self.entity
    }

    /// Render a child entity into `area`.
    pub fn render_entity<E: Component<A>>(&self, entity: Entity<E>, frame: &mut Frame, area: Rect) {
        let cx = RenderContext {
            store: self.store,
            focused: self.focused,
            entity: Some(entity.id()),
        };
        self.store.render_entity(entity.id(), frame, area, &cx);
    }
}

/// Whether an action was handled or should continue propagating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStatus {
    /// The action was consumed; stop propagation.
    Handled,
    /// The action was ignored; pass it to the parent or root.
    Continue,
}

/// A self-contained piece of UI.
///
/// `A` is the application's action type. Actions are synchronous semantic
/// intents and bubble through the active overlay/focus path. Deferred work is
/// represented explicitly by the context APIs: `notify`/`observe` reads
/// current state again, `emit`/`subscribe` communicates typed events,
/// `subscribe_stream` consumes an external stream, and `spawn` delivers a
/// one-shot typed result.
pub trait Component<A: 'static>: 'static {
    /// Called once before the first frame.
    fn init(&mut self, _cx: &mut Context<'_, Self, A>)
    where
        Self: Sized,
    {
    }

    /// Handle a synchronous semantic action.
    fn handle_action(&mut self, _action: &A, _cx: &mut Context<'_, Self, A>) -> ActionStatus
    where
        Self: Sized,
    {
        ActionStatus::Continue
    }

    /// Called before the entity is removed (overlay closed, parent removed).
    /// Use it to abort streams or tasks and drop handles; `emit` here is
    /// dropped because the source is gone, so reach the parent with
    /// `update`/`dispatch` instead.
    fn cleanup(&mut self, _cx: &mut Context<'_, Self, A>)
    where
        Self: Sized,
    {
    }

    /// Render the component's current state. Rendering must not perform I/O
    /// or mutate state.
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, A>);
}
