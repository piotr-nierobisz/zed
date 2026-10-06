//! Tells Claude what the user is looking at: the file and selection in the
//! editor they last focused.

use crate::{ClaudeCode, file_uri};
use editor::{Editor, EditorEvent};
use gpui::{Context, Focusable as _, Window};
use language::Point;
use serde::Serialize;
use std::time::Duration;

/// How long to wait after a selection change before telling Claude, so that
/// typing and dragging send one update rather than dozens.
const NOTIFICATION_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SelectionInfo {
    text: String,
    file_path: String,
    file_url: String,
    selection: SelectionRange,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SelectionRange {
    start: Position,
    end: Position,
    is_empty: bool,
}

/// Zero-based, with `character` counted in UTF-16 code units as in LSP.
#[derive(Clone, Debug, Serialize)]
struct Position {
    line: u32,
    character: u32,
}

pub(crate) fn observe_editors(cx: &mut Context<ClaudeCode>) {
    cx.observe_new(|editor: &mut Editor, window, cx| {
        let Some(window) = window else {
            return;
        };
        if !editor.mode().is_full() {
            return;
        }
        cx.subscribe_in(
            &cx.entity(),
            window,
            |editor, _, event: &EditorEvent, window, cx| {
                if matches!(
                    event,
                    EditorEvent::SelectionsChanged { local: true } | EditorEvent::Focused
                ) {
                    editor_selection_changed(editor, window, cx);
                }
            },
        )
        .detach();
    })
    .detach();
}

fn editor_selection_changed(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    if !editor.focus_handle(cx).is_focused(window) {
        return;
    }
    let Some(claude_code) = ClaudeCode::global(cx) else {
        return;
    };
    let Some(info) = selection_info(editor, cx) else {
        return;
    };
    claude_code.update(cx, |this, cx| this.selection_changed(info, cx));
}

fn selection_info(editor: &mut Editor, cx: &mut Context<Editor>) -> Option<SelectionInfo> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let abs_path = buffer.read(cx).file()?.as_local()?.abs_path(cx);
    let display_snapshot = editor.display_snapshot(cx);
    let selection = editor.selections.newest::<Point>(&display_snapshot);
    let snapshot = display_snapshot.buffer_snapshot();
    let text = snapshot
        .text_for_range(selection.start..selection.end)
        .collect::<String>();
    let start = snapshot.point_to_point_utf16(selection.start);
    let end = snapshot.point_to_point_utf16(selection.end);

    Some(SelectionInfo {
        text,
        file_path: abs_path.to_string_lossy().into_owned(),
        file_url: file_uri(&abs_path),
        selection: SelectionRange {
            start: Position {
                line: start.row,
                character: start.column,
            },
            end: Position {
                line: end.row,
                character: end.column,
            },
            is_empty: selection.start == selection.end,
        },
    })
}

impl ClaudeCode {
    fn selection_changed(&mut self, info: SelectionInfo, cx: &mut Context<Self>) {
        self.latest_selection = Some(info);
        if self.connections.is_empty() || self.selection_notification.is_some() {
            return;
        }
        self.selection_notification = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTIFICATION_DELAY).await;
            this.update(cx, |this, _| {
                this.selection_notification = None;
                if let Some(info) = &this.latest_selection
                    && let Ok(params) = serde_json::to_value(info)
                {
                    this.broadcast_notification("selection_changed", params);
                }
            })
            .ok();
        }));
    }

    pub(crate) fn latest_selection(&self) -> Option<&SelectionInfo> {
        self.latest_selection.as_ref()
    }
}
