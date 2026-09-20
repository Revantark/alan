//! Session lifecycle commands: `/new`, `/fork`, `/effort`, `/rename`,
//! `/summarize-new`, and the steering flow.

use super::ChatView;
use crate::root::AlanAction;
use crate::views::components::{ForkEvent, ForkOverlay};
use alan_tui::context::Context;

/// `/new`: reset to a fresh, empty session.
pub(crate) fn start_new_session(view: &mut ChatView, cx: &mut Context<'_, ChatView, AlanAction>) {
    if view.controller.is_busy() {
        return;
    }
    let agent = view.controller.agent();
    cx.spawn(
        async move {
            agent
                .reset_session()
                .await
                .map_err(|error| alan_tui::TaskError(Box::new(error)))
        },
        move |result, view, _cx| match result {
            Ok(()) => {
                view.controller.clear_transcript();
                view.controller.push_info("Started a new session.");
            }
            Err(error) => view
                .controller
                .push_info(format!("failed to start new session: {error}")),
        },
    );
}

/// `/fork`: open the fork overlay.
pub(crate) fn request_fork(
    view: &mut ChatView,
    text: &str,
    cx: &mut Context<'_, ChatView, AlanAction>,
) {
    if view.controller.is_busy() {
        view.controller
            .push_info("cannot fork while a response is streaming".to_owned());
        return;
    }
    if let Some((_, args)) = crate::core::SlashCommand::parse_with_args(text)
        && !args.trim().is_empty()
    {
        view.controller.push_info("usage: /fork".to_owned());
        return;
    }
    open_fork_overlay(view, cx);
}

/// `/effort`: set the reasoning effort.
pub(crate) fn apply_effort(
    view: &mut ChatView,
    text: &str,
    cx: &mut Context<'_, ChatView, AlanAction>,
) {
    if view.controller.is_busy() {
        return;
    }
    let agent = view.controller.agent();

    let effort = match crate::core::SlashCommand::parse_with_args(text).map(|(_, args)| args) {
        Some(args) => match crate::core::SlashCommand::parse_effort(args) {
            Some(effort) => effort,
            None => {
                view.controller.push_info(
                    "usage: /effort <none|minimal|low|medium|high|xhigh|max>".to_owned(),
                );
                return;
            }
        },
        None => return,
    };

    let agent = agent.clone();
    cx.spawn(
        async move {
            agent
                .set_reasoning_effort(effort)
                .await
                .map_err(|error| alan_tui::TaskError(Box::new(error)))?;

            super::persist_reasoning_effort(effort)
                .await
                .map_err(|error| alan_tui::TaskError(error.into()))?;

            Ok::<_, alan_tui::TaskError>(format!("reasoning effort set to {effort}"))
        },
        move |result, view, cx| match result {
            Ok(msg) => {
                view.controller.set_reasoning_effort(effort);
                view.controller.push_info(msg);
                cx.notify();
            }
            Err(error) => {
                view.controller
                    .push_info(format!("failed to set reasoning effort: {error}"));
                cx.notify();
            }
        },
    );
}

/// `/rename`: rename the current session.
pub(crate) fn rename_session(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    text: &str,
) {
    let args = crate::core::SlashCommand::parse_with_args(text)
        .map(|(_, args)| args.trim().to_owned())
        .filter(|args| !args.is_empty());
    let name = match args {
        Some(name) => name,
        None => {
            view.controller
                .push_info("usage: /rename <name>".to_owned());
            return;
        }
    };
    let agent = view.controller.agent();
    cx.spawn(
        async move {
            agent
                .rename_session(&name)
                .await
                .map_err(|error| alan_tui::TaskError(Box::new(error)))?;

            Ok::<_, alan_tui::TaskError>(format!("Session renamed to {name}"))
        },
        move |result, view, cx| match result {
            Ok(msg) => {
                view.controller.push_info(msg);
                cx.notify();
            }
            Err(error) => {
                view.controller
                    .push_info(format!("failed to rename session: {error}"));
                cx.notify();
            }
        },
    );
}

/// `/summarize-new`: summarize the session into a new one.
pub(crate) fn start_summarize_new(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    text: &str,
) {
    if view.controller.is_busy() {
        return;
    }
    let agent = view.controller.agent();
    let focus = crate::core::SlashCommand::parse_with_args(text)
        .map(|(_, args)| args.trim().to_owned())
        .filter(|args| !args.is_empty());

    view.controller.set_loading(Some("summarizing".to_owned()));
    view.sync_status(cx);

    cx.spawn(
        async move {
            let summary = agent
                .summarize(focus.as_deref())
                .await
                .map_err(|error| alan_tui::TaskError(Box::new(error)))?;
            let seed = vec![agent::AgentMessage::user(format!(
                "Session handoff — continue from this state:\n\n{summary}"
            ))];
            agent
                .reset_session_with(seed)
                .await
                .map_err(|error| alan_tui::TaskError(Box::new(error)))?;
            Ok::<(), alan_tui::TaskError>(())
        },
        move |result, view, cx| {
            view.controller.set_loading(None);
            view.controller.clear_transcript();
            match result {
                Ok(()) => view.controller.push_info("Summarized into a new session."),
                Err(error) => view
                    .controller
                    .push_info(format!("failed to summarize session: {error}")),
            }
            view.sync_status(cx);
        },
    );
}

/// Open the `/fork` overlay.
pub(crate) fn open_fork_overlay(view: &mut ChatView, cx: &mut Context<'_, ChatView, AlanAction>) {
    let agent = view.controller.agent();
    view.fork_in_flight = true;
    let picker = cx.open_overlay(ForkOverlay::new(std::sync::Arc::clone(&agent)));
    cx.subscribe_once::<crate::views::components::ForkEvent, crate::views::components::ForkOverlay, _>(picker, move |_event, view, _picker, cx| {
        let ForkEvent::Chosen { end_index } = *_event else {
            // Closing the picker without choosing must release the
            // latch, otherwise every later `/fork` is rejected.
            view.fork_in_flight = false;
            return;
        };
        let agent = view.controller.agent();
        let _ = cx.spawn(
            async move {
                agent
                    .fork_session(end_index)
                    .await
                    .map_err(|e| alan_tui::TaskError(Box::new(e)))?;
                let messages = agent.messages().await;
                Ok::<_, alan_tui::TaskError>(messages)
            },
            move |result, view, cx| {
                view.fork_in_flight = false;
                match result {
                    Ok(messages) => {
                        view.controller.apply_restored(
                            messages,
                            llm::Usage::default(),
                            view.controller.model_name(),
                            view.controller.max_context(),
                        );
                        view.controller.push_info("forked session".to_owned());
                        if let Some(editor) = view.editor {
                            let prompts = super::recall_prompts(view.controller.entries());
                            cx.update(editor, |e| e.seed_history(prompts));
                        }
                    }
                    Err(error) => {
                        view.controller
                            .push_info(format!("failed to fork: {error}"));
                    }
                }
                cx.notify();
            },
        );
    });
}
