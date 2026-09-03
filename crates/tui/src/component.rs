//! Component trait and render context.
//!
//! A component owns local state and renders its current state. It receives
//! two kinds of application-defined input:
//!
//! - actions (`A`) are semantic user intents routed synchronously through the
//!   active overlay, focused component, parent components, and root;
//! - messages (`M`) are deferred internal communication delivered to a
//!   component or the root.
//!
//! Actions can stop or continue propagation. Messages are delivered to their
//! target and do not bubble. Rendering is pure: it reads state, recalculates
//! layout, and never performs I/O or mutation.
//!
//! Components never see raw crossterm events; a
//! [`KeyMapper`](crate::keymap::KeyMapper) converts native events into
//! actions.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::context::Context;
use crate::entity::{Entity, EntityId, EntityStore};

/// Read-only capabilities available during render.
///
/// Rendering must not mutate state or perform I/O, so the render context
/// exposes entity state read-only: a parent can render child entities, but
/// nothing can be mutated from a render pass.
pub struct RenderContext<'a, A: 'static, M: 'static = ()> {
    pub(crate) store: &'a EntityStore<A, M>,
    /// The entity that currently holds focus, if any.
    pub(crate) focused: Option<EntityId>,
    /// The entity this context renders (None only inside the runtime).
    pub(crate) entity: Option<EntityId>,
}

impl<'a, A, M> RenderContext<'a, A, M> {
    pub(crate) fn new(
        store: &'a EntityStore<A, M>,
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
    ///
    /// Components use this to draw visible focus styling (highlighted
    /// borders, cursors) without tracking focus state themselves.
    pub fn is_focused(&self) -> bool {
        self.entity.is_some() && self.focused == self.entity
    }

    /// Render a child entity into `area`.
    ///
    /// Parents calculate child areas and delegate rendering to their
    /// children through this method.
    pub fn render_entity<E: Component<A, M>>(
        &self,
        entity: Entity<E>,
        frame: &mut Frame,
        area: Rect,
    ) {
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
    /// The action was ignored; pass it to the parent or global handler.
    Continue,
}

/// A self-contained piece of UI.
///
/// `A` is the application's action type. Actions represent semantic input
/// intents, such as "quit", "submit", or "move selection". They are routed
/// synchronously through the active overlay, focused component, its parents,
/// and finally the root. [`Self::handle_action`] returns whether routing
/// should stop or continue.
///
/// `M` is the application's message type. Messages represent deferred
/// communication between components and background tasks. They are delivered
/// after the current callback returns through [`Self::handle_message`].
/// Messages are targeted and do not bubble through the component hierarchy.
///
/// `render` draws the current state into `area`.
pub trait Component<A: 'static, M: 'static = ()>: 'static {
    /// Called once before the first frame, after the component is
    /// registered. Use it to insert children and set initial focus with
    /// [`Context::focus_entity`]. Entities inserted here are available
    /// before the first render.
    fn init(&mut self, _cx: &mut Context<'_, Self, A, M>)
    where
        Self: Sized,
    {
    }

    /// Handle a semantic action routed to this component.
    ///
    /// Actions originate from the application's
    /// [`KeyMapper`](crate::keymap::KeyMapper) and are delivered
    /// synchronously. Return [`ActionStatus::Handled`] to stop routing, or
    /// [`ActionStatus::Continue`] to let the action reach the parent or root.
    ///
    /// The default implementation ignores every action.
    fn handle_action(&mut self, _action: &A, _cx: &mut Context<'_, Self, A, M>) -> ActionStatus
    where
        Self: Sized,
    {
        ActionStatus::Continue
    }

    /// Handle a deferred message delivered to this component.
    ///
    /// Messages may be emitted by another component or produced by a
    /// background task. Unlike actions, messages do not participate in
    /// propagation and have no handled/unhandled result.
    ///
    /// The default implementation ignores every message.
    fn handle_message(&mut self, _message: M, _cx: &mut Context<'_, Self, A, M>)
    where
        Self: Sized,
    {
    }

    /// Render the component's current state.
    ///
    /// Recalculate layout every frame; never store `Frame` or `Rect` values,
    /// never perform I/O, never mutate state.
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, A, M>);
}
