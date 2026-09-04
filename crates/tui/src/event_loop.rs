//! The event loop. Actions route synchronously; all worker results, typed
//! events, and observations are delivered in deferred non-reentrant batches.

use crate::component::ActionStatus;
use crate::context::{Ctx, RuntimeState};
use crate::entity::{EntityId, EntityStore};
use crate::error::RuntimeError;
use crate::keymap::{InputContext, KeyMapper};
use crate::render;
use crate::subscription::{
    EventDelivery, RuntimeDelivery, StreamDelivery, StreamDeliveryEvent, SubscriptionRecord,
};
use crate::task::TaskDelivery;
use crate::terminal::TerminalGuard;
use crossterm::event::EventStream;
use futures_util::StreamExt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

fn dispatch<A: 'static>(
    action: &A,
    core: &mut RuntimeState<A>,
    store: &EntityStore<A>,
    root: EntityId,
) {
    let initial = core.input_target();
    if core.overlays.is_active() {
        if let Some(id) = initial {
            let mut cx = Ctx::new(core, store, id);
            let _ = store.dispatch_action(id, action, &mut cx);
        }
        return;
    }
    let mut target = initial;
    while let Some(id) = target {
        let mut cx = Ctx::new(core, store, id);
        if store.dispatch_action(id, action, &mut cx) != Some(ActionStatus::Continue) {
            return;
        }
        if id == root {
            return;
        }
        target = core.parent_map.get(&id).copied();
    }
    let mut cx = Ctx::new(core, store, root);
    let _ = store.dispatch_action(root, action, &mut cx);
}

fn flush_requests<A: 'static>(core: &mut RuntimeState<A>, store: &mut EntityStore<A>) {
    loop {
        while let Some((id, slot)) = core.pending_inserts.pop_front() {
            store.insert_slot(id, slot);
        }
        for (id, slot) in std::mem::take(&mut core.pending_overlays) {
            store.insert_slot(id, slot);
            core.focus.save();
            let mut cx = Ctx::new(core, store, id);
            store.init_if_needed(id, &mut cx);
            core.overlays.push(id);
        }
        for id in std::mem::take(&mut core.pending_inits) {
            let mut cx = Ctx::new(core, store, id);
            store.init_if_needed(id, &mut cx);
        }
        if core.pending_inserts.is_empty()
            && core.pending_overlays.is_empty()
            && core.pending_inits.is_empty()
        {
            break;
        }
    }
}

fn close_overlays<A: 'static>(core: &mut RuntimeState<A>, store: &mut EntityStore<A>) {
    while core.pending_closes > 0 {
        core.pending_closes -= 1;
        let Some(id) = core.overlays.pop() else {
            continue;
        };
        core.focus.restore();
        remove_entity_tree(core, store, id);
    }
}

fn remove_entity_tree<A: 'static>(
    core: &mut RuntimeState<A>,
    store: &mut EntityStore<A>,
    root: EntityId,
) {
    let mut removed = vec![root];
    let mut index = 0;
    while index < removed.len() {
        let parent = removed[index];
        let children: Vec<_> = core
            .parent_map
            .iter()
            .filter_map(|(child, owner)| (*owner == parent).then_some(*child))
            .collect();
        removed.extend(children);
        index += 1;
    }
    // Children first so a parent can still reach its children during
    // cleanup; only entities that ran `init` are cleaned up.
    for id in removed.iter().rev() {
        let mut cx = Ctx::new(core, store, *id);
        store.cleanup_if_needed(*id, &mut cx);
    }
    for id in removed {
        core.focus.remove_entity(id);
        core.parent_map.remove(&id);
        store.remove_entity(id);
        core.invalidated.remove(&id);
    }
    core.cleanup_subscriptions(store);
}

fn deliver_task<A: 'static>(
    delivery: TaskDelivery,
    core: &mut RuntimeState<A>,
    store: &EntityStore<A>,
) {
    let Some(handler) = core.task_handlers.remove(&delivery.id) else {
        return;
    };
    if handler.is_active() && store.is_active_entity(delivery.target) {
        handler.invoke(delivery.result, core, store);
    }
}

fn deliver_event<A: 'static>(
    delivery: EventDelivery,
    core: &mut RuntimeState<A>,
    store: &EntityStore<A>,
) {
    // Snapshot ids so callbacks can cancel/remove subscriptions safely.
    let ids: Vec<_> = core
        .subscriptions
        .iter()
        .filter_map(|(id, record)| match record {
            SubscriptionRecord::Event(event)
                if event.source == delivery.source
                    && event.handler.event_type() == delivery.event_type =>
            {
                Some(*id)
            }
            _ => None,
        })
        .collect();
    let mut ids = ids;
    ids.sort_unstable();
    for id in ids {
        let Some(SubscriptionRecord::Event(mut record)) = core.subscriptions.remove(&id) else {
            continue;
        };
        if record.active.load(Ordering::Acquire)
            && store.is_active_entity(record.target)
            && store.is_active_entity(record.source)
        {
            record.handler.invoke(
                delivery.event.as_ref(),
                record.target,
                record.source,
                core,
                store,
            );
        }
        if record.active.load(Ordering::Acquire)
            && store.is_active_entity(record.target)
            && store.is_active_entity(record.source)
        {
            core.subscriptions
                .insert(id, SubscriptionRecord::Event(record));
        } else {
            record.active.store(false, Ordering::Release);
            let _ = record.cancellation.send(true);
        }
    }
}

fn deliver_observations<A: 'static>(
    source: EntityId,
    core: &mut RuntimeState<A>,
    store: &EntityStore<A>,
) {
    if !store.is_active_entity(source) {
        return;
    }
    let ids: Vec<_> = core
        .subscriptions
        .iter()
        .filter_map(|(id, record)| match record {
            SubscriptionRecord::Observation(observation) if observation.source == source => {
                Some(*id)
            }
            _ => None,
        })
        .collect();
    let mut ids = ids;
    ids.sort_unstable();
    for id in ids {
        let Some(SubscriptionRecord::Observation(mut record)) = core.subscriptions.remove(&id)
        else {
            continue;
        };
        if record.active.load(Ordering::Acquire) && store.is_active_entity(record.target) {
            record
                .handler
                .invoke(record.target, record.source, core, store);
        }
        if record.active.load(Ordering::Acquire)
            && store.is_active_entity(record.target)
            && store.is_active_entity(record.source)
        {
            core.subscriptions
                .insert(id, SubscriptionRecord::Observation(record));
        } else {
            record.active.store(false, Ordering::Release);
            let _ = record.cancellation.send(true);
        }
    }
}

fn deliver_stream<A: 'static>(
    delivery: StreamDelivery,
    core: &mut RuntimeState<A>,
    store: &EntityStore<A>,
) {
    let Some(record) = core.subscriptions.remove(&delivery.id) else {
        return;
    };
    let SubscriptionRecord::Stream(mut record) = record else {
        return;
    };
    if !record.active.load(Ordering::Acquire) || !store.is_active_entity(record.target) {
        record.active.store(false, Ordering::Release);
        return;
    }
    let closed = matches!(delivery.event, StreamDeliveryEvent::Closed);
    match delivery.event {
        StreamDeliveryEvent::Item(item) => record.handler.invoke(item, record.target, core, store),
        StreamDeliveryEvent::Closed => record.handler.close(record.target, core, store),
    }
    if !closed && record.active.load(Ordering::Acquire) && store.is_active_entity(record.target) {
        core.subscriptions
            .insert(delivery.id, SubscriptionRecord::Stream(record));
    } else {
        record.active.store(false, Ordering::Release);
        let _ = record.cancellation.send(true);
    }
}

pub(crate) async fn event_loop<A>(
    mut guard: TerminalGuard,
    root: EntityId,
    entity_store: Arc<Mutex<EntityStore<A>>>,
    mut state: RuntimeState<A>,
    key_mapper: Arc<dyn KeyMapper<A>>,
    mut rx: mpsc::UnboundedReceiver<RuntimeDelivery>,
    tick_rate: Duration,
) -> Result<(), RuntimeError>
where
    A: Send + 'static,
{
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(tick_rate);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        {
            let mut store = entity_store.lock().expect("entity store poisoned");
            // Insertions must precede delivery so newly created entities can
            // be initialized. Deferred deliveries are processed before close
            // requests: an overlay may emit its result and close in the same
            // action callback.
            flush_requests(&mut state, &mut store);
            // Snapshot invalidations before callbacks run. Invalidations caused
            // by an observer/event callback belong to the next batch.
            let invalidated = state.take_invalidated();
            deliver_deferred_batch(&mut state, &store);
            for source in invalidated {
                deliver_observations(source, &mut state, &store);
            }
            close_overlays(&mut state, &mut store);
            state.cleanup_subscriptions(&store);
            if state.take_dirty() {
                let focused = state.focus.current();
                guard
                    .terminal()
                    .draw(|frame| render::draw(root, &state.overlays, &store, frame, focused))?;
            }
        }
        let input_context = InputContext {
            overlay_active: state.overlays.is_active(),
            focus_active: state.focus.current().is_some(),
        };
        tokio::select! {
            maybe_event = events.next() => {
                let Some(result) = maybe_event else { break };
                let event = result?;
                if let Some(action) = key_mapper.map(&event, &input_context) {
                    let store = entity_store.lock().expect("entity store poisoned");
                    dispatch(&action, &mut state, &store, root);
                }
            },
            _ = tick.tick() => {}
            delivery = rx.recv() => {
                let Some(delivery) = delivery else { break };
                state.deliveries.push_back(delivery);
            }
        }
        if state.should_quit() {
            break;
        }
    }
    Ok(())
}

fn deliver_deferred_batch<A: 'static>(core: &mut RuntimeState<A>, store: &EntityStore<A>) {
    let batch = std::mem::take(&mut core.deliveries);
    for delivery in batch {
        match delivery {
            RuntimeDelivery::Task(delivery) => deliver_task(delivery, core, store),
            RuntimeDelivery::Stream(delivery) => deliver_stream(delivery, core, store),
            RuntimeDelivery::Event(delivery) => deliver_event(delivery, core, store),
            RuntimeDelivery::Observation(_) => {}
        }
    }
}
