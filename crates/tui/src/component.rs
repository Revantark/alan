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

use crossterm::event::MouseEvent;
use ratatui::Frame;
use ratatui::layout::Rect;
use std::any::Any;

use crate::context::Context;
use crate::entity::{Entity, EntityId, EntityStore};

/// Read-only capabilities available during render.
///
/// `'cx` is the lifetime of the current render pass. Ancestors can provide
/// state to their descendants via
/// [`render_with_state`](Self::render_with_state); the reference is
/// guaranteed to live for the child's render call because a child's render
/// always happens synchronously inside its parent's render.
pub struct RenderContext<'a, 'cx, A: 'static> {
    pub(crate) store: &'a EntityStore<A>,
    /// The entity that currently holds focus, if any.
    pub(crate) focused: Option<EntityId>,
    /// The entity this context renders.
    pub(crate) entity: Option<EntityId>,
    /// State provided by an ancestor for this render pass, if any.
    pub(crate) state: Option<&'cx dyn Any>,
}

impl<'a, 'cx, A> RenderContext<'a, 'cx, A> {
    pub(crate) fn new(
        store: &'a EntityStore<A>,
        focused: Option<EntityId>,
        entity: Option<EntityId>,
    ) -> Self {
        Self {
            store,
            focused,
            entity,
            state: None,
        }
    }

    /// Whether the entity being rendered currently holds focus.
    pub fn is_focused(&self) -> bool {
        self.entity.is_some() && self.focused == self.entity
    }

    /// Render a child entity into `area`.
    ///
    /// State provided by an ancestor remains visible to the child and its
    /// descendants, unless the child itself calls
    /// [`render_with_state`](Self::render_with_state) (which shadows the
    /// outer state for its own descendants).
    pub fn render_entity<E: Component<A>>(
        &self,
        entity: Entity<E>,
        frame: &mut Frame,
        area: Rect,
    ) {
        let cx = RenderContext {
            store: self.store,
            focused: self.focused,
            entity: Some(entity.id()),
            state: self.state,
        };
        self.store.render_entity(entity.id(), frame, area, &cx);
    }

    /// Render a child entity into `area`, providing `state` for the child
    /// and its descendants.
    ///
    /// The child can read it with [`state`](Self::state) during its render
    /// call. Because a child's render is always a synchronous call nested
    /// inside the parent's render, the borrow of `state` lives exactly as
    /// long as the child's render — the compiler checks this via the `'cx`
    /// lifetime, so no runtime or unsafe machinery is involved. Providing a
    /// new state shadows the one seen by this context's entity, but only for
    /// the child's subtree.
    pub fn render_with_state<S: 'static, E: Component<A>>(
        &self,
        entity: Entity<E>,
        frame: &mut Frame,
        area: Rect,
        state: &'cx S,
    ) {
        let cx = RenderContext {
            store: self.store,
            focused: self.focused,
            entity: Some(entity.id()),
            state: Some(state),
        };
        self.store.render_entity(entity.id(), frame, area, &cx);
    }

    /// The state provided by an ancestor for this render pass, if it is of
    /// type `S`. Returns `None` when no ancestor provided state or when the
    /// provided state is of a different type.
    pub fn state<S: 'static>(&self) -> Option<&'cx S> {
        self.state?.downcast_ref::<S>()
    }

    /// Like [`state`](Self::state), but panics when an ancestor did not
    /// provide state of type `S`.
    ///
    /// Use this in components that cannot render without their state — a
    /// missing or mistyped provider becomes an immediate, named error
    /// instead of a silent fallback. Components that tolerate absence
    /// should use [`state`](Self::state) instead.
    pub fn expect_state<S: 'static>(&self) -> &'cx S {
        match self.state::<S>() {
            Some(state) => state,
            None => panic!(
                "no state of type {} provided; the parent must render this \
                 component with render_with_state passing a &{}",
                std::any::type_name::<S>(),
                std::any::type_name::<S>(),
            ),
        }
    }

    /// Read another entity's state during render. The target's slot is locked
    /// briefly and released; this performs no I/O or mutation, so it stays
    /// within the render contract. Reading the entity currently being rendered
    /// returns `None`: its slot is already held by the render pass, so
    /// re-locking it would deadlock.
    pub fn read<E: 'static, R>(&self, target: Entity<E>, f: impl FnOnce(&E) -> R) -> Option<R> {
        if Some(target.id()) == self.entity {
            None
        } else {
            self.store.typed_read(target.id(), f)
        }
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

    /// Handle a mouse event that hit this component's rendered area.
    ///
    /// Mouse events are routed by area: the framework hit-tests the pointer
    /// position against the areas recorded during the last render pass and
    /// delivers the event to the topmost component under the pointer. Return
    /// [`ActionStatus::Continue`] to let the event bubble to the parent.
    ///
    /// Unlike `handle_action`, the event is a raw crossterm [`MouseEvent`]:
    /// mouse geometry (columns, rows, drag state) is inherently positional and
    /// has no meaningful semantic mapping.
    fn handle_mouse(
        &mut self,
        _mouse: MouseEvent,
        _area: Rect,
        _cx: &mut Context<'_, Self, A>,
    ) -> ActionStatus
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
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, A>);
}
