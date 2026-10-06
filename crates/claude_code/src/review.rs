//! Reviewing the file edits Claude proposes, in place.
//!
//! The CLI sends the whole new file (`openDiff`) and waits. The proposal goes
//! straight into the file's buffer, tracked by an [`ActionLog`] the same way
//! agent edits are, so every change shows inline with its own Accept and
//! Reject controls. Nothing is written to disk while the review is open, and
//! autosave leaves the buffer alone. Once every change is decided, the file is
//! saved if anything was accepted and the CLI is told the outcome.

use crate::{ClaudeCode, ConnectionId, dedicated_pane, mcp::text_content, open_file};
use action_log::ActionLog;
use agent_ui::{Keep, KeepAll, Reject, RejectAll};
use anyhow::{Context as _, Result, anyhow};
use buffer_diff::{BufferDiff, DiffHunkStatus};
use editor::{DiffHunkRenderer, Editor, MultiBufferSnapshot, SelectionEffects, scroll::Autoscroll};
use futures::channel::oneshot;
use gpui::{
    Action, AnyElement, AnyWindowHandle, App, AppContext as _, Context, Entity, Focusable as _,
    KeyContext, Subscription, Task, WeakEntity, Window,
};
use language::{Buffer, BufferEditSource, BufferEvent, DiskState, Point};
use multi_buffer::{MultiBufferDiffHunk, MultiBufferRow};
use serde::Deserialize;
use serde_json::Value;
use std::{ops::Range, path::PathBuf, sync::Arc, time::Duration};
use ui::{ButtonStyle, TintColor, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ItemHandle, Pane, Workspace};

#[derive(Deserialize)]
pub(crate) struct OpenDiffArgs {
    new_file_path: String,
    new_file_contents: String,
    tab_name: String,
}

#[derive(Deserialize)]
pub(crate) struct CloseTabArgs {
    tab_name: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReviewId(usize);

impl ReviewId {
    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

pub(crate) struct PendingDiff {
    id: ReviewId,
    /// The name the CLI uses to refer to this proposal. Only unique per
    /// connection: two sessions editing files with the same name use the same one.
    tab_name: String,
    connection_id: ConnectionId,
    window: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
    buffer: Entity<Buffer>,
    action_log: Entity<ActionLog>,
    /// Set once the action log has computed the changes to review.
    diff: Option<Entity<BufferDiff>>,
    original_text: String,
    was_dirty: bool,
    accepted_any: bool,
    editors: Vec<ReviewingEditor>,
    respond: Option<oneshot::Sender<DiffOutcome>>,
    _subscriptions: Vec<Subscription>,
}

struct ReviewingEditor {
    editor: WeakEntity<Editor>,
    _actions: Vec<Subscription>,
}

/// How long an accepted proposal waits for the CLI to write it before Zed
/// goes back to what is on disk.
const CLI_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// An accepted proposal the CLI is about to write. Until it does, the buffer
/// holds the accepted text unsaved, with autosave held off.
pub(crate) struct AwaitingWrite {
    connection_id: ConnectionId,
    buffer: Entity<Buffer>,
    editors: Vec<WeakEntity<Editor>>,
    /// The file as it was when the proposal was accepted; a change means the
    /// CLI has written it.
    disk_state: Option<DiskState>,
    original_text: String,
    was_dirty: bool,
    _subscription: Subscription,
    _timeout: Task<()>,
}

enum DiffOutcome {
    /// Something was accepted; carries the file as saved.
    Saved(String),
    Rejected,
    /// Withdrawn without a decision in Zed, e.g. because the user answered
    /// in the terminal.
    Closed,
}

impl DiffOutcome {
    fn into_content(self, tab_name: &str) -> Vec<Value> {
        match self {
            Self::Saved(text) => vec![text_content("FILE_SAVED"), text_content(text)],
            Self::Rejected => vec![text_content("DIFF_REJECTED"), text_content(tab_name)],
            Self::Closed => vec![text_content("TAB_CLOSED")],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    Accept,
    Reject,
}

enum Scope {
    Hunk(Range<editor::Anchor>),
    Selections,
    All,
}

impl PendingDiff {
    pub(crate) fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    fn editors_for_buffer(&self, cx: &App) -> Vec<Entity<Editor>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        workspace
            .read(cx)
            .items_of_type::<Editor>(cx)
            .filter(|editor| {
                editor.read(cx).buffer().read(cx).as_singleton().as_ref() == Some(&self.buffer)
            })
            .collect()
    }

    fn show_in_editor(
        &mut self,
        editor: Entity<Editor>,
        scroll_to_first_change: bool,
        claude_code: WeakEntity<ClaudeCode>,
        cx: &mut App,
    ) {
        let weak_editor = editor.downgrade();
        if self
            .editors
            .iter()
            .any(|reviewing| reviewing.editor == weak_editor)
        {
            return;
        }
        let Some(diff) = self.diff.clone() else {
            return;
        };
        let review_id = self.id;
        let renderer: Arc<dyn DiffHunkRenderer> = Arc::new(ReviewHunkRenderer {
            claude_code: claude_code.clone(),
            review_id,
        });
        let actions = self.window.update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor
                    .buffer()
                    .update(cx, |multibuffer, cx| multibuffer.add_diff(diff, cx));
                editor.set_diff_hunk_renderer(Some(renderer), cx);
                editor.set_expand_all_diff_hunks(cx);
                editor.register_addon(ReviewAddon);
                editor.register_addon(HoldAutosave);
                if scroll_to_first_change {
                    select_first_change(editor, window, cx);
                }
                vec![
                    register_review_action::<Keep>(
                        editor,
                        &weak_editor,
                        &claude_code,
                        review_id,
                        Decision::Accept,
                        false,
                    ),
                    register_review_action::<Reject>(
                        editor,
                        &weak_editor,
                        &claude_code,
                        review_id,
                        Decision::Reject,
                        false,
                    ),
                    register_review_action::<KeepAll>(
                        editor,
                        &weak_editor,
                        &claude_code,
                        review_id,
                        Decision::Accept,
                        true,
                    ),
                    register_review_action::<RejectAll>(
                        editor,
                        &weak_editor,
                        &claude_code,
                        review_id,
                        Decision::Reject,
                        true,
                    ),
                ]
            })
        });
        if let Some(actions) = actions.log_err() {
            self.editors.push(ReviewingEditor {
                editor: weak_editor,
                _actions: actions,
            });
        }
    }

    /// Takes the review controls out of the editors, and returns the editors,
    /// which still hold off autosave until [`release_autosave`].
    fn hide(&mut self, cx: &mut App) -> Vec<WeakEntity<Editor>> {
        let mut editors = Vec::new();
        for reviewing in self.editors.drain(..) {
            if let Some(editor) = reviewing.editor.upgrade() {
                editor.update(cx, |editor, cx| {
                    // Also brings back the editor's git diff.
                    editor.set_diff_hunk_renderer(None, cx);
                    editor.unregister_addon::<ReviewAddon>();
                });
            }
            editors.push(reviewing.editor);
        }
        editors
    }

    fn file_exists(&self, cx: &App) -> bool {
        self.buffer
            .read(cx)
            .file()
            .is_some_and(|file| file.disk_state().exists())
    }
}

impl ClaudeCode {
    pub(crate) fn open_diff(
        &mut self,
        connection_id: ConnectionId,
        args: OpenDiffArgs,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<Value>>> {
        let abs_path = PathBuf::from(&args.new_file_path);
        let Some((window, workspace)) = self.workspace_for_path(&abs_path, cx) else {
            return Task::ready(Err(anyhow!("no Zed window is open")));
        };
        self.open_diff_in(connection_id, window, workspace, args, cx)
    }

    fn open_diff_in(
        &mut self,
        connection_id: ConnectionId,
        window: AnyWindowHandle,
        workspace: Entity<Workspace>,
        args: OpenDiffArgs,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<Value>>> {
        let abs_path = PathBuf::from(&args.new_file_path);
        cx.spawn(async move |this, cx| {
            let open = window.update(cx, |_, window, cx| {
                let pane = this.update(cx, |this, cx| this.review_pane(&workspace, window, cx))?;
                anyhow::Ok(workspace.update(cx, |workspace, cx| {
                    open_file(
                        workspace,
                        abs_path.clone(),
                        Some(pane.downgrade()),
                        true,
                        window,
                        cx,
                    )
                }))
            })??;
            let item = open.await?;
            let editor = item
                .downcast::<Editor>()
                .with_context(|| format!("{} is not a text file", abs_path.display()))?;
            let buffer = editor
                .read_with(cx, |editor, cx| editor.buffer().read(cx).as_singleton())
                .with_context(|| format!("{} is not a text file", abs_path.display()))?;

            let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
            let old_text = snapshot.text();
            // Buffers keep `\n` line endings whatever the file uses.
            let new_text = args.new_file_contents.replace("\r\n", "\n");
            let edits = cx
                .background_executor()
                .spawn(async move { language::text_diff(&old_text, &new_text) })
                .await;

            let (respond, outcome) = oneshot::channel();
            this.update(cx, |this, cx| {
                this.begin_review(
                    connection_id,
                    args.tab_name.clone(),
                    window,
                    &workspace,
                    buffer,
                    &snapshot,
                    edits,
                    respond,
                    cx,
                )
            })??;
            let outcome = outcome.await.unwrap_or(DiffOutcome::Closed);
            Ok(outcome.into_content(&args.tab_name))
        })
    }

    fn begin_review(
        &mut self,
        connection_id: ConnectionId,
        tab_name: String,
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        buffer: Entity<Buffer>,
        snapshot: &language::BufferSnapshot,
        edits: Vec<(Range<usize>, Arc<str>)>,
        respond: oneshot::Sender<DiffOutcome>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self
                .pending_diffs
                .iter()
                .any(|pending| pending.buffer == buffer),
            "another change to this file is waiting for review in Zed"
        );
        if edits.is_empty() {
            respond.send(DiffOutcome::Saved(snapshot.text())).ok();
            return Ok(());
        }
        if let Some(ix) = self.pending_diffs.iter().position(|pending| {
            pending.connection_id == connection_id && pending.tab_name == tab_name
        }) {
            let replaced = self.pending_diffs.remove(ix);
            self.withdraw_diff(replaced, cx);
        }

        let id = self.next_review_id;
        self.next_review_id = id.next();
        let project = workspace.read(cx).project().clone();
        let action_log = cx.new(|_| ActionLog::new(project));
        let edits = edits
            .into_iter()
            .map(|(range, text)| (snapshot.anchor_range_inside(range), text))
            .collect::<Vec<_>>();
        let original_text = buffer.read(cx).text();
        let was_dirty = match self.take_awaited_write(&buffer, cx) {
            Some(was_dirty) => was_dirty,
            None => buffer.read(cx).is_dirty(),
        };

        // The action log only attributes the edit to the agent when the
        // baseline is recorded first and the edit is reported in the same
        // update; otherwise it treats the proposal as the user's own typing.
        action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
        buffer.update(cx, |buffer, cx| {
            buffer.start_transaction();
            buffer.edit(edits, None, cx);
            buffer.end_transaction_with_source(BufferEditSource::Agent, cx);
        });
        action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));

        let subscriptions = vec![
            cx.observe(&action_log, move |this, _, cx| this.review_changed(id, cx)),
            cx.subscribe(
                workspace,
                move |this, _, event: &workspace::Event, cx| match event {
                    workspace::Event::ItemAdded { item } => {
                        this.item_added(id, item.as_ref(), cx)
                    }
                    workspace::Event::ItemRemoved { .. } => this.item_removed(id, cx),
                    _ => {}
                },
            ),
        ];
        self.pending_diffs.push(PendingDiff {
            id,
            tab_name,
            connection_id,
            window,
            workspace: workspace.downgrade(),
            buffer,
            action_log,
            diff: None,
            original_text,
            was_dirty,
            accepted_any: false,
            editors: Vec::new(),
            respond: Some(respond),
            _subscriptions: subscriptions,
        });
        Ok(())
    }

    /// Proposals all open in one pane, split off the active one the first time
    /// and again whenever the user has closed it.
    fn review_pane(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Pane> {
        dedicated_pane(&mut self.review_panes, workspace, window, cx)
    }

    fn review_position(&self, id: ReviewId) -> Option<usize> {
        self.pending_diffs.iter().position(|pending| pending.id == id)
    }

    fn review_changed(&mut self, id: ReviewId, cx: &mut Context<Self>) {
        let Some(ix) = self.review_position(id) else {
            return;
        };
        let pending = &mut self.pending_diffs[ix];
        let diff = pending
            .action_log
            .read(cx)
            .changed_buffers(cx)
            .find(|(buffer, _)| *buffer == pending.buffer)
            .map(|(_, diff)| diff);
        match diff {
            Some(diff) => {
                if pending.diff.is_none() {
                    pending.diff = Some(diff);
                    let claude_code = cx.weak_entity();
                    for editor in pending.editors_for_buffer(cx) {
                        pending.show_in_editor(editor, true, claude_code.clone(), cx);
                    }
                }
            }
            // No changes left to review; the diff is only computed after the
            // proposal is applied, so wait until it has been seen at least once.
            None => {
                if pending.diff.is_some() {
                    let pending = self.pending_diffs.remove(ix);
                    self.finish_review(pending, cx);
                }
            }
        }
    }

    fn item_added(&mut self, id: ReviewId, item: &dyn ItemHandle, cx: &mut Context<Self>) {
        let Some(editor) = item.downcast::<Editor>() else {
            return;
        };
        let claude_code = cx.weak_entity();
        let Some(ix) = self.review_position(id) else {
            return;
        };
        let pending = &mut self.pending_diffs[ix];
        if pending.diff.is_some()
            && editor.read(cx).buffer().read(cx).as_singleton().as_ref() == Some(&pending.buffer)
        {
            pending.show_in_editor(editor, false, claude_code, cx);
        }
    }

    /// Closing the file hands the decision back to the terminal.
    fn item_removed(&mut self, id: ReviewId, cx: &mut Context<Self>) {
        let Some(ix) = self.review_position(id) else {
            return;
        };
        if self.pending_diffs[ix].editors_for_buffer(cx).is_empty() {
            let pending = self.pending_diffs.remove(ix);
            self.withdraw_diff(pending, cx);
        }
    }

    fn review_hunks(
        &mut self,
        id: ReviewId,
        editor: &Entity<Editor>,
        scope: Scope,
        decision: Decision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.review_position(id) else {
            return;
        };
        let pending = &mut self.pending_diffs[ix];
        let Some(diff) = pending.diff.clone() else {
            return;
        };
        let buffer_id = pending.buffer.read(cx).remote_id();
        let hunks = editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let ranges = match &scope {
                Scope::Hunk(range) => vec![range.start..range.start],
                Scope::Selections => editor.selections.disjoint_anchor_ranges().collect(),
                Scope::All => vec![editor::Anchor::Min..editor::Anchor::Max],
            };
            let hunks = editor
                .diff_hunks_in_ranges(&ranges, &snapshot)
                .filter(|hunk| hunk.buffer_id == buffer_id)
                .collect::<Vec<_>>();
            if !matches!(scope, Scope::All) {
                move_to_next_change(editor, &snapshot, &hunks, window, cx);
            }
            hunks
        });
        if hunks.is_empty() {
            return;
        }

        match decision {
            Decision::Accept => {
                pending.accepted_any = true;
                for hunk in &hunks {
                    pending.action_log.update(cx, |log, cx| {
                        log.keep_edits_in_range(
                            pending.buffer.clone(),
                            hunk.buffer_range.clone(),
                            None,
                            cx,
                        )
                    });
                }
            }
            // Put the original text back as an ordinary edit, which the action
            // log reconciles with the proposal. Unlike its own reject, this
            // doesn't save the file.
            Decision::Reject => {
                let base_text = diff.read(cx).base_text(cx);
                let edits = hunks
                    .iter()
                    .map(|hunk| {
                        let base_range =
                            hunk.diff_base_byte_range.start.0..hunk.diff_base_byte_range.end.0;
                        (
                            hunk.buffer_range.clone(),
                            base_text.text_for_range(base_range).collect::<String>(),
                        )
                    })
                    .collect::<Vec<_>>();
                pending
                    .buffer
                    .update(cx, |buffer, cx| buffer.edit(edits, None, cx));
            }
        }
    }

    fn finish_review(&mut self, mut pending: PendingDiff, cx: &mut Context<Self>) {
        let editors = pending.hide(cx);
        let Some(respond) = pending.respond.take() else {
            release_autosave(&editors, cx);
            return;
        };
        let text = pending.buffer.read(cx).text();
        if pending.accepted_any {
            // The CLI writes the file itself, once it has checked the file is
            // still as Claude read it, so saving here would make that check fail.
            respond.send(DiffOutcome::Saved(text)).ok();
            self.await_cli_write(pending, editors, cx);
        } else {
            release_autosave(&editors, cx);
            // Nothing reached the disk, so only the unsaved-changes marker the
            // proposal left behind has to go.
            if !pending.was_dirty && text == pending.original_text && pending.file_exists(cx) {
                // The reload runs on its own; nothing needs to wait for it.
                drop(pending.buffer.update(cx, |buffer, cx| buffer.reload(cx)));
            }
            respond.send(DiffOutcome::Rejected).ok();
        }
    }

    /// Takes a proposal back out of the buffer without a decision.
    pub(crate) fn withdraw_diff(&mut self, mut pending: PendingDiff, cx: &mut Context<Self>) {
        let editors = pending.hide(cx);
        release_autosave(&editors, cx);
        restore_original(&pending.buffer, &pending.original_text, pending.was_dirty, cx);
        if let Some(respond) = pending.respond.take() {
            respond.send(DiffOutcome::Closed).ok();
        }
    }

    fn await_cli_write(
        &mut self,
        pending: PendingDiff,
        editors: Vec<WeakEntity<Editor>>,
        cx: &mut Context<Self>,
    ) {
        let buffer = pending.buffer;
        let subscription = cx.subscribe(&buffer, |this, buffer, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::FileHandleChanged) {
                this.written_by_cli(&buffer, cx);
            }
        });
        let timeout = cx.spawn({
            let buffer = buffer.downgrade();
            async move |this, cx| {
                cx.background_executor().timer(CLI_WRITE_TIMEOUT).await;
                this.update(cx, |this, cx| {
                    if let Some(buffer) = buffer.upgrade() {
                        this.abandon_awaited_write(&buffer, cx);
                    }
                })
                .ok();
            }
        });
        let disk_state = buffer.read(cx).file().map(|file| file.disk_state());
        self.awaiting_writes.push(AwaitingWrite {
            connection_id: pending.connection_id,
            buffer,
            editors,
            disk_state,
            original_text: pending.original_text,
            was_dirty: pending.was_dirty,
            _subscription: subscription,
            _timeout: timeout,
        });
    }

    fn written_by_cli(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        let Some(ix) = self
            .awaiting_writes
            .iter()
            .position(|awaiting| &awaiting.buffer == buffer)
        else {
            return;
        };
        let disk_state = buffer.read(cx).file().map(|file| file.disk_state());
        if disk_state == self.awaiting_writes[ix].disk_state {
            return;
        }
        let awaiting = self.awaiting_writes.remove(ix);
        release_autosave(&awaiting.editors, cx);
        // The file now holds the accepted text; loading it clears the
        // unsaved-changes marker.
        drop(buffer.update(cx, |buffer, cx| buffer.reload(cx)));
    }

    /// Gives up on the CLI writing an accepted proposal, e.g. because it was
    /// interrupted, and goes back to what is on disk.
    fn abandon_awaited_write(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        let Some(ix) = self
            .awaiting_writes
            .iter()
            .position(|awaiting| &awaiting.buffer == buffer)
        else {
            return;
        };
        let awaiting = self.awaiting_writes.remove(ix);
        release_autosave(&awaiting.editors, cx);
        restore_original(&awaiting.buffer, &awaiting.original_text, awaiting.was_dirty, cx);
    }

    pub(crate) fn abandon_awaited_writes(
        &mut self,
        connection_id: Option<ConnectionId>,
        cx: &mut Context<Self>,
    ) {
        let buffers = self
            .awaiting_writes
            .iter()
            .filter(|awaiting| connection_id.is_none_or(|id| awaiting.connection_id == id))
            .map(|awaiting| awaiting.buffer.clone())
            .collect::<Vec<_>>();
        for buffer in buffers {
            self.abandon_awaited_write(&buffer, cx);
        }
    }

    /// Claude's next edit to a file can arrive before Zed has noticed the CLI
    /// writing the previous one. Returns whether the buffer had unsaved changes
    /// before that earlier proposal.
    fn take_awaited_write(&mut self, buffer: &Entity<Buffer>, cx: &mut App) -> Option<bool> {
        let ix = self
            .awaiting_writes
            .iter()
            .position(|awaiting| &awaiting.buffer == buffer)?;
        let awaiting = self.awaiting_writes.remove(ix);
        release_autosave(&awaiting.editors, cx);
        Some(awaiting.was_dirty)
    }

    pub(crate) fn close_tab(
        &mut self,
        connection_id: ConnectionId,
        args: CloseTabArgs,
        cx: &mut Context<Self>,
    ) -> Vec<Value> {
        if let Some(ix) = self.pending_diffs.iter().position(|pending| {
            pending.connection_id == connection_id && pending.tab_name == args.tab_name
        }) {
            let pending = self.pending_diffs.remove(ix);
            self.withdraw_diff(pending, cx);
        }
        vec![text_content("TAB_CLOSED")]
    }

    /// Withdraws every proposal from one connection; the CLI does this before
    /// each prompt, so it mustn't touch another session's.
    pub(crate) fn close_all_diff_tabs(
        &mut self,
        connection_id: ConnectionId,
        cx: &mut Context<Self>,
    ) -> Vec<Value> {
        let (closed, pending) = std::mem::take(&mut self.pending_diffs)
            .into_iter()
            .partition::<Vec<_>, _>(|pending| pending.connection_id == connection_id);
        self.pending_diffs = pending;
        let count = closed.len();
        for pending in closed {
            self.withdraw_diff(pending, cx);
        }
        vec![text_content(format!("CLOSED_{count}_DIFF_TABS"))]
    }
}

fn register_review_action<A: Action>(
    editor: &mut Editor,
    weak_editor: &WeakEntity<Editor>,
    claude_code: &WeakEntity<ClaudeCode>,
    review_id: ReviewId,
    decision: Decision,
    all: bool,
) -> Subscription {
    let weak_editor = weak_editor.clone();
    let claude_code = claude_code.clone();
    editor.register_action(move |_: &A, window, cx| {
        let (Some(claude_code), Some(editor)) = (claude_code.upgrade(), weak_editor.upgrade())
        else {
            return;
        };
        let scope = if all { Scope::All } else { Scope::Selections };
        claude_code.update(cx, |this, cx| {
            this.review_hunks(review_id, &editor, scope, decision, window, cx)
        });
    })
}

fn select_first_change(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let first_change = editor
        .diff_hunks_in_ranges(&[editor::Anchor::Min..editor::Anchor::Max], &snapshot)
        .next()
        .map(|hunk| hunk.multi_buffer_range.start);
    if let Some(start) = first_change {
        editor.change_selections(
            SelectionEffects::scroll(Autoscroll::center()),
            window,
            cx,
            |selections| selections.select_anchor_ranges([start..start]),
        );
    }
}

/// After deciding on the change under the cursor, moves on to the next one.
fn move_to_next_change(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    decided: &[MultiBufferDiffHunk],
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let cursor = editor
        .selections
        .newest::<Point>(&editor.display_snapshot(cx))
        .head();
    if !decided
        .iter()
        .any(|hunk| hunk.row_range.contains(&MultiBufferRow(cursor.row)))
    {
        return;
    }
    let next = decided
        .last()
        .and_then(|last| {
            editor
                .diff_hunks_in_ranges(&[last.multi_buffer_range.end..editor::Anchor::Max], snapshot)
                .nth(1)
        })
        .or_else(|| {
            let first = decided.first()?;
            editor
                .diff_hunks_in_ranges(&[editor::Anchor::Min..first.multi_buffer_range.start], snapshot)
                .next()
        })
        .map(|hunk| hunk.multi_buffer_range.start);
    if let Some(start) = next {
        editor.change_selections(
            SelectionEffects::scroll(Autoscroll::fit()),
            window,
            cx,
            |selections| selections.select_anchor_ranges([start..start]),
        );
    }
}

/// Marks an editor as reviewing a proposal, so the agent review keybindings apply.
struct ReviewAddon;

impl editor::Addon for ReviewAddon {
    fn extend_key_context(&self, key_context: &mut KeyContext, _: &App) {
        key_context.add("agent_diff");
        key_context.add("editor_agent_diff");
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Keeps autosave off a buffer holding text that isn't Zed's to write: a
/// proposal under review, or an accepted one the CLI is about to write.
struct HoldAutosave;

impl editor::Addon for HoldAutosave {
    fn suppresses_autosave(&self) -> bool {
        true
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn release_autosave(editors: &[WeakEntity<Editor>], cx: &mut App) {
    for editor in editors {
        if let Some(editor) = editor.upgrade() {
            editor.update(cx, |editor, _| editor.unregister_addon::<HoldAutosave>());
        }
    }
}

/// Puts back the text from before a proposal. When nothing else was unsaved,
/// reloading also clears the unsaved-changes marker, and picks up the file if
/// the CLI has written it meanwhile.
fn restore_original(buffer: &Entity<Buffer>, original_text: &str, was_dirty: bool, cx: &mut App) {
    let current_text = buffer.read(cx).text();
    if current_text != original_text {
        let edits = language::text_diff(&current_text, original_text);
        buffer.update(cx, |buffer, cx| buffer.edit(edits, None, cx));
    }
    let file_exists = buffer
        .read(cx)
        .file()
        .is_some_and(|file| file.disk_state().exists());
    if !was_dirty && file_exists {
        drop(buffer.update(cx, |buffer, cx| buffer.reload(cx)));
    }
}

struct ReviewHunkRenderer {
    claude_code: WeakEntity<ClaudeCode>,
    review_id: ReviewId,
}

impl ReviewHunkRenderer {
    fn decision_button(
        &self,
        row: u32,
        decision: Decision,
        hunk_range: &Range<editor::Anchor>,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> Button {
        let (id, label, tooltip, action): (_, _, _, Box<dyn Action>) = match decision {
            Decision::Accept => ("claude-accept", "Accept", "Accept Change", Box::new(Keep)),
            Decision::Reject => ("claude-reject", "Reject", "Reject Change", Box::new(Reject)),
        };
        let focus_handle = editor.focus_handle(cx);
        let claude_code = self.claude_code.clone();
        let review_id = self.review_id;
        let editor = editor.clone();
        let hunk_range = hunk_range.clone();
        Button::new((id, row as u64), label)
            .label_size(LabelSize::Small)
            .when(decision == Decision::Accept, |button| {
                button.style(ButtonStyle::Tinted(TintColor::Success))
            })
            .tooltip(move |_window, cx| {
                Tooltip::for_action_in(tooltip, action.as_ref(), &focus_handle, cx)
            })
            .on_click(move |_, window, cx| {
                if let Some(claude_code) = claude_code.upgrade() {
                    claude_code.update(cx, |this, cx| {
                        this.review_hunks(
                            review_id,
                            &editor,
                            Scope::Hunk(hunk_range.clone()),
                            decision,
                            window,
                            cx,
                        )
                    });
                }
            })
    }
}

impl DiffHunkRenderer for ReviewHunkRenderer {
    fn render_hunk_controls(
        &self,
        row: u32,
        _status: &DiffHunkStatus,
        hunk_range: Range<editor::Anchor>,
        _is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        // Drop shadows render as a dark halo on transparent windows.
        let opaque_window =
            cx.theme().window_background_appearance() == gpui::WindowBackgroundAppearance::Opaque;

        h_flex()
            .h(line_height)
            .mr_0p5()
            .gap_1()
            .px_0p5()
            .pb_1()
            .border_x_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .rounded_b_md()
            .bg(cx.theme().colors().editor_background)
            .block_mouse_except_scroll()
            .when(opaque_window, |this| this.shadow_md())
            .child(self.decision_button(row, Decision::Reject, &hunk_range, editor, cx))
            .child(self.decision_button(row, Decision::Accept, &hunk_range, editor, cx))
            .into_any_element()
    }

    fn render_hunk_as_staged(&self, _status: &DiffHunkStatus, _cx: &App) -> bool {
        false
    }

    fn show_controls_for_all_hunks(&self) -> bool {
        true
    }

    fn place_controls_below_hunk(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClaudeCodeSettings;
    use gpui::{TestAppContext, UpdateGlobal as _, VisualTestContext};
    use project::{FakeFs, Fs as _, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use workspace::{Item as _, MultiWorkspace, SplitDirection};

    const TAB_NAME: &str = "✻ [Claude Code] greeter.py ⧉";

    const ORIGINAL: &str = "\
def greet(name):
    return \"Hello, \" + name


def farewell(name):
    return \"Goodbye, \" + name
";

    const PROPOSED: &str = "\
def greet(name):
    return f\"Hello, {name}\"


def farewell(name):
    \"\"\"Say goodbye.\"\"\"
    return f\"Goodbye, {name}\"
";

    /// The first change accepted and the second rejected.
    const FIRST_CHANGE_ONLY: &str = "\
def greet(name):
    return f\"Hello, {name}\"


def farewell(name):
    return \"Goodbye, \" + name
";

    struct Review<'a> {
        fs: Arc<FakeFs>,
        claude_code: Entity<ClaudeCode>,
        workspace: Entity<Workspace>,
        window: AnyWindowHandle,
        editor: Entity<Editor>,
        outcome: Task<Result<Vec<Value>>>,
        cx: &'a mut VisualTestContext,
    }

    impl Review<'_> {
        fn propose(&mut self, contents: &str, tab_name: &str) -> Task<Result<Vec<Value>>> {
            propose(
                &self.claude_code,
                self.window,
                &self.workspace,
                contents,
                tab_name,
                self.cx,
            )
        }

        /// Writes the file the way the CLI does once a proposal is accepted.
        async fn cli_writes(&mut self, text: &str) {
            self.fs
                .insert_file(path!("/project/greeter.py"), text.as_bytes().to_vec())
                .await;
            self.cx.run_until_parked();
        }

        fn buffer(&mut self) -> Entity<Buffer> {
            self.editor
                .read_with(self.cx, |editor, cx| editor.buffer().read(cx).as_singleton())
                .unwrap()
        }

        /// The file's text; the editor also shows the lines a change removes.
        fn text(&mut self) -> String {
            self.buffer().read_with(self.cx, |buffer, _| buffer.text())
        }

        /// Whether the tab shows unsaved changes.
        fn is_dirty(&mut self) -> bool {
            let buffer_is_dirty = self
                .buffer()
                .read_with(self.cx, |buffer, _| buffer.is_dirty());
            let tab_is_dirty = self
                .editor
                .read_with(self.cx, |editor, cx| editor.is_dirty(cx));
            assert_eq!(tab_is_dirty, buffer_is_dirty);
            tab_is_dirty
        }

        fn suppresses_autosave(&mut self) -> bool {
            self.editor
                .read_with(self.cx, |editor, cx| editor.suppresses_autosave(cx))
        }

        async fn file_on_disk(&self) -> String {
            self.fs
                .load(path!("/project/greeter.py").as_ref())
                .await
                .unwrap()
        }

        fn changes(&mut self) -> Vec<Range<editor::Anchor>> {
            self.editor.update(self.cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                editor
                    .diff_hunks_in_ranges(&[editor::Anchor::Min..editor::Anchor::Max], &snapshot)
                    .map(|hunk| hunk.multi_buffer_range)
                    .collect()
            })
        }

        /// Clicks one of the buttons under a change.
        fn decide(&mut self, change: Range<editor::Anchor>, decision: Decision) {
            let editor = self.editor.clone();
            let claude_code = self.claude_code.clone();
            self.cx.update(|window, cx| {
                claude_code.update(cx, |this, cx| {
                    let id = this.pending_diffs[0].id;
                    this.review_hunks(id, &editor, Scope::Hunk(change), decision, window, cx)
                })
            });
            self.cx.run_until_parked();
        }

        fn is_pending(&mut self) -> bool {
            self.claude_code
                .read_with(self.cx, |this, _| !this.pending_diffs.is_empty())
        }

        /// Checks the proposal was taken back out of the buffer without a decision.
        async fn assert_withdrawn(&mut self) {
            let outcome = (&mut self.outcome).await.unwrap();
            assert_eq!(outcome, vec![text_content("TAB_CLOSED")]);
            assert_eq!(self.text(), ORIGINAL);
            assert_eq!(self.file_on_disk().await, ORIGINAL);
            assert!(!self.is_dirty());
            assert!(!self.suppresses_autosave());
        }
    }

    async fn propose_edit(cx: &mut TestAppContext) -> Review<'_> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            workspace::register_project_item::<Editor>(cx);
            // Keep the tests off the network.
            SettingsStore::update_global(cx, |store, _| {
                store.override_global(ClaudeCodeSettings { enabled: false })
            });
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "greeter.py": ORIGINAL }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let claude_code = cx.new(ClaudeCode::new);
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });
        let window = cx.update(|window, _| window.window_handle());

        let outcome = propose(&claude_code, window, &workspace, PROPOSED, TAB_NAME, cx);
        let editor = workspace
            .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))
            .expect("the file should be open");

        Review {
            fs,
            claude_code,
            workspace,
            window,
            editor,
            outcome,
            cx,
        }
    }

    /// Sends `openDiff` the way the CLI does.
    fn propose(
        claude_code: &Entity<ClaudeCode>,
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        contents: &str,
        tab_name: &str,
        cx: &mut VisualTestContext,
    ) -> Task<Result<Vec<Value>>> {
        let outcome = claude_code.update(cx, |this, cx| {
            this.open_diff_in(
                ConnectionId::default(),
                window,
                workspace.clone(),
                OpenDiffArgs {
                    new_file_path: path!("/project/greeter.py").to_string(),
                    new_file_contents: contents.to_string(),
                    tab_name: tab_name.to_string(),
                },
                cx,
            )
        });
        cx.run_until_parked();
        outcome
    }

    #[gpui::test]
    async fn test_proposal_waits_in_the_buffer(cx: &mut TestAppContext) {
        let mut review = propose_edit(cx).await;

        assert_eq!(review.text(), PROPOSED);
        assert_eq!(review.changes().len(), 2);
        assert!(review.suppresses_autosave());
        assert_eq!(review.file_on_disk().await, ORIGINAL);
    }

    #[gpui::test]
    async fn test_accepting_part_of_a_proposal(cx: &mut TestAppContext) {
        let mut review = propose_edit(cx).await;
        let changes = review.changes();

        review.decide(changes[0].clone(), Decision::Accept);
        assert_eq!(review.changes().len(), 1, "the other change still awaits a decision");
        assert_eq!(review.file_on_disk().await, ORIGINAL);

        review.decide(changes[1].clone(), Decision::Reject);
        let outcome = (&mut review.outcome).await.unwrap();
        assert_eq!(
            outcome,
            vec![text_content("FILE_SAVED"), text_content(FIRST_CHANGE_ONLY)]
        );
        // Writing the file is the CLI's job: it checks nothing else changed it
        // since Claude read it.
        assert_eq!(review.file_on_disk().await, ORIGINAL);
        assert_eq!(review.text(), FIRST_CHANGE_ONLY);
        assert!(review.suppresses_autosave());

        review.cli_writes(FIRST_CHANGE_ONLY).await;
        assert_eq!(review.text(), FIRST_CHANGE_ONLY);
        assert!(!review.is_dirty());
        assert!(!review.suppresses_autosave());
    }

    #[gpui::test]
    async fn test_accepted_proposal_the_cli_never_writes(cx: &mut TestAppContext) {
        let mut review = propose_edit(cx).await;

        review.cx.dispatch_action(KeepAll);
        review.cx.run_until_parked();
        (&mut review.outcome).await.unwrap();
        assert_eq!(review.text(), PROPOSED);

        review.cx.executor().advance_clock(CLI_WRITE_TIMEOUT);
        review.cx.run_until_parked();
        assert_eq!(review.text(), ORIGINAL);
        assert_eq!(review.file_on_disk().await, ORIGINAL);
        assert!(!review.is_dirty());
        assert!(!review.suppresses_autosave());
    }

    #[gpui::test]
    async fn test_next_edit_arriving_before_the_write_is_noticed(cx: &mut TestAppContext) {
        let mut review = propose_edit(cx).await;
        review.cx.dispatch_action(KeepAll);
        review.cx.run_until_parked();
        (&mut review.outcome).await.unwrap();

        // Claude's next edit to the file, based on what the CLI just wrote.
        let next_proposal = PROPOSED.replace("Say goodbye.", "Say goodbye politely.");
        let mut next_outcome = review.propose(&next_proposal, "second");
        assert!(review.is_pending());

        // Zed only now sees the earlier write, and must not reload over the
        // proposal under review.
        review.cli_writes(PROPOSED).await;
        assert_eq!(review.text(), next_proposal);
        assert!(review.is_pending());
        assert_eq!(review.changes().len(), 1);

        review.cx.dispatch_action(KeepAll);
        review.cx.run_until_parked();
        let outcome = (&mut next_outcome).await.unwrap();
        assert_eq!(
            outcome,
            vec![text_content("FILE_SAVED"), text_content(next_proposal.clone())]
        );
        review.cli_writes(&next_proposal).await;
        assert!(!review.is_dirty());
        assert!(!review.suppresses_autosave());
    }

    #[gpui::test]
    async fn test_rejecting_a_whole_proposal(cx: &mut TestAppContext) {
        let mut review = propose_edit(cx).await;

        for change in review.changes() {
            review.decide(change, Decision::Reject);
        }
        let outcome = (&mut review.outcome).await.unwrap();
        assert_eq!(
            outcome,
            vec![text_content("DIFF_REJECTED"), text_content(TAB_NAME)]
        );
        assert_eq!(review.text(), ORIGINAL);
        assert_eq!(review.file_on_disk().await, ORIGINAL);
        assert!(!review.is_dirty());
        assert!(!review.suppresses_autosave());
    }

    #[gpui::test]
    async fn test_answering_in_the_terminal_withdraws_the_proposal(cx: &mut TestAppContext) {
        let mut review = propose_edit(cx).await;

        let closed = review.claude_code.update(review.cx, |this, cx| {
            this.close_tab(
                ConnectionId::default(),
                CloseTabArgs {
                    tab_name: TAB_NAME.to_string(),
                },
                cx,
            )
        });
        review.cx.run_until_parked();

        assert_eq!(closed, vec![text_content("TAB_CLOSED")]);
        review.assert_withdrawn().await;
    }

    #[gpui::test]
    async fn test_other_sessions_leave_the_proposal_alone(cx: &mut TestAppContext) {
        let mut review = propose_edit(cx).await;

        // Every session withdraws its own proposals before each prompt.
        let closed = review.claude_code.update(review.cx, |this, cx| {
            this.close_all_diff_tabs(ConnectionId::default().next(), cx)
        });
        review.cx.run_until_parked();

        assert_eq!(closed, vec![text_content("CLOSED_0_DIFF_TABS")]);
        assert!(review.is_pending());
        assert_eq!(review.text(), PROPOSED);
    }

    #[gpui::test]
    async fn test_closing_the_file_withdraws_the_proposal(cx: &mut TestAppContext) {
        let mut review = propose_edit(cx).await;

        let editor_id = review.editor.entity_id();
        let workspace = review
            .editor
            .read_with(review.cx, |editor, _| editor.workspace())
            .unwrap();
        workspace
            .update_in(review.cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_item_by_id(editor_id, workspace::SaveIntent::Skip, window, cx)
                })
            })
            .await
            .unwrap();
        review.cx.run_until_parked();

        assert!(!review.is_pending());
        review.assert_withdrawn().await;
    }

    #[gpui::test]
    async fn test_accepting_everything_from_the_keyboard(cx: &mut TestAppContext) {
        let mut review = propose_edit(cx).await;

        review.cx.dispatch_action(KeepAll);
        review.cx.run_until_parked();

        let outcome = (&mut review.outcome).await.unwrap();
        assert_eq!(
            outcome,
            vec![text_content("FILE_SAVED"), text_content(PROPOSED)]
        );
        review.cli_writes(PROPOSED).await;
        assert_eq!(review.text(), PROPOSED);
        assert!(!review.is_dirty());
    }

    #[gpui::test]
    async fn test_proposals_open_in_a_pane_of_their_own(cx: &mut TestAppContext) {
        let mut review = propose_edit(cx).await;
        let review_pane = review.workspace.read_with(review.cx, |workspace, _| {
            assert_eq!(workspace.panes().len(), 2, "the proposal split off a new pane");
            workspace.active_pane().clone()
        });
        review.cx.dispatch_action(KeepAll);
        review.cx.run_until_parked();
        (&mut review.outcome).await.unwrap();
        review.cli_writes(PROPOSED).await;

        // Back to work in the other pane; the next proposal still goes to the
        // review pane.
        review.workspace.update_in(review.cx, |workspace, window, cx| {
            workspace.activate_pane_in_direction(SplitDirection::Left, window, cx)
        });
        let next_proposal = PROPOSED.replace("Say goodbye.", "Say goodbye politely.");
        let _next_outcome = review.propose(&next_proposal, "second");
        review.workspace.read_with(review.cx, |workspace, _| {
            assert_eq!(workspace.panes().len(), 2);
            assert_eq!(workspace.active_pane(), &review_pane);
        });
        assert_eq!(review.text(), next_proposal);
    }
}
