//! Lets the `claude` CLI use Zed as its IDE.
//!
//! Zed runs the same local MCP-over-WebSocket server the Claude Code
//! extensions for VS Code and JetBrains run, and advertises it through a lock
//! file in `~/.claude/ide`. A `claude` session started in one of the open
//! projects finds it with `/ide` (or automatically, in Zed's terminal). Every
//! file edit Claude asks permission for then shows up inline in the editor,
//! with Accept and Reject controls on each change, and Claude continues once
//! each change has been decided.

mod claude_code_settings;
mod cli_terminal;
mod mcp;
mod review;
mod selection;
mod server;

use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::channel::mpsc;
use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, Global, Subscription, Task,
    TaskExt as _, WeakEntity, Window,
};
use serde_json::Value;
use settings::{Settings as _, SettingsStore};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use terminal::TerminalExtraEnv;
use util::ResultExt as _;
use workspace::{
    AppState, ItemHandle, MultiWorkspace, OpenOptions, OpenVisible, Pane, SplitDirection, Workspace,
};

pub use claude_code_settings::ClaudeCodeSettings;

/// Tells a `claude` started in Zed's terminal which server to connect to.
const PORT_ENV_VAR: &str = "CLAUDE_CODE_SSE_PORT";
const IDE_INTEGRATION_ENV_VAR: &str = "ENABLE_IDE_INTEGRATION";

pub fn init(cx: &mut App) {
    let claude_code = cx.new(ClaudeCode::new);
    cx.set_global(GlobalClaudeCode(claude_code));
}

struct GlobalClaudeCode(Entity<ClaudeCode>);

impl Global for GlobalClaudeCode {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct ConnectionId(usize);

struct RunningServer {
    auth_token: Arc<str>,
    port: Option<u16>,
    lock_file_path: Option<PathBuf>,
    _task: Task<()>,
}

pub struct ClaudeCode {
    server: Option<RunningServer>,
    connections: HashMap<ConnectionId, mpsc::UnboundedSender<String>>,
    next_connection_id: ConnectionId,
    pending_diffs: Vec<review::PendingDiff>,
    awaiting_writes: Vec<review::AwaitingWrite>,
    /// The pane each workspace shows proposals in.
    review_panes: Vec<(WeakEntity<Workspace>, WeakEntity<Pane>)>,
    /// The pane each workspace runs `claude` in.
    terminal_panes: Vec<(WeakEntity<Workspace>, WeakEntity<Pane>)>,
    next_review_id: review::ReviewId,
    latest_selection: Option<selection::SelectionInfo>,
    selection_notification: Option<Task<()>>,
    lock_file_update: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ClaudeCode {
    fn new(cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe_global::<SettingsStore>(|this, cx| this.apply_settings(cx)),
            cx.on_app_quit(|this, _| {
                this.remove_lock_file();
                async {}
            }),
        ];
        selection::observe_editors(cx);
        cli_terminal::observe_workspaces(cx);
        observe_workspace_folders(cx);

        let mut this = Self {
            server: None,
            connections: HashMap::default(),
            next_connection_id: ConnectionId::default(),
            pending_diffs: Vec::new(),
            awaiting_writes: Vec::new(),
            review_panes: Vec::new(),
            terminal_panes: Vec::new(),
            next_review_id: review::ReviewId::default(),
            latest_selection: None,
            selection_notification: None,
            lock_file_update: None,
            _subscriptions: subscriptions,
        };
        this.apply_settings(cx);
        this
    }

    pub(crate) fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalClaudeCode>()
            .map(|global| global.0.clone())
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let enabled = ClaudeCodeSettings::get_global(cx).enabled;
        if enabled && self.server.is_none() {
            self.start_server(cx);
        } else if !enabled && self.server.is_some() {
            self.stop_server(cx);
        }
    }

    fn start_server(&mut self, cx: &mut Context<Self>) {
        let auth_token: Arc<str> = uuid::Uuid::new_v4().to_string().into();
        let task = cx.spawn({
            let auth_token = auth_token.clone();
            async move |this, cx| {
                if let Err(error) = server::run(this.clone(), auth_token, cx).await {
                    log::error!("Claude Code IDE server stopped: {error:#}");
                }
            }
        });
        self.server = Some(RunningServer {
            auth_token,
            port: None,
            lock_file_path: None,
            _task: task,
        });
    }

    fn stop_server(&mut self, cx: &mut Context<Self>) {
        self.remove_lock_file();
        self.server = None;
        // Dropping the senders closes the sockets.
        self.connections.clear();
        for pending in std::mem::take(&mut self.pending_diffs) {
            self.withdraw_diff(pending, cx);
        }
        self.abandon_awaited_writes(None, cx);
        if let Some(env) = cx.try_global::<TerminalExtraEnv>() {
            let mut env = env.clone();
            env.0.remove(PORT_ENV_VAR);
            env.0.remove(IDE_INTEGRATION_ENV_VAR);
            cx.set_global(env);
        }
    }

    fn server_started(&mut self, port: u16, cx: &mut Context<Self>) {
        let Some(server) = self.server.as_mut() else {
            return;
        };
        server.port = Some(port);
        server.lock_file_path = Some(lock_file_dir().join(format!("{port}.lock")));
        log::info!("Claude Code IDE server listening on port {port}");

        let env = &mut cx.default_global::<TerminalExtraEnv>().0;
        env.insert(PORT_ENV_VAR.to_string(), port.to_string());
        env.insert(IDE_INTEGRATION_ENV_VAR.to_string(), "true".to_string());

        self.update_lock_file(cx);
    }

    fn add_connection(&mut self, outgoing: mpsc::UnboundedSender<String>) -> ConnectionId {
        let id = self.next_connection_id;
        self.next_connection_id = id.next();
        self.connections.insert(id, outgoing);
        id
    }

    fn remove_connection(&mut self, connection_id: ConnectionId, cx: &mut Context<Self>) {
        self.connections.remove(&connection_id);
        let (closed, pending) = std::mem::take(&mut self.pending_diffs)
            .into_iter()
            .partition(|pending| pending.connection_id() == connection_id);
        self.pending_diffs = pending;
        for pending in closed {
            self.withdraw_diff(pending, cx);
        }
        // A session that's gone won't write anything more.
        self.abandon_awaited_writes(Some(connection_id), cx);
    }

    fn send(&self, connection_id: ConnectionId, message: Value) {
        if let Some(connection) = self.connections.get(&connection_id) {
            connection.unbounded_send(message.to_string()).ok();
        }
    }

    fn broadcast_notification(&self, method: &str, params: Value) {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        })
        .to_string();
        for connection in self.connections.values() {
            connection.unbounded_send(message.clone()).ok();
        }
    }

    /// Rewrites the lock file shortly after the open projects change, so that
    /// `claude` started in a newly opened folder can find Zed.
    fn schedule_lock_file_update(&mut self, cx: &mut Context<Self>) {
        self.lock_file_update = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            this.update(cx, |this, cx| this.update_lock_file(cx)).ok();
        }));
    }

    fn update_lock_file(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.server.as_ref() else {
            return;
        };
        let Some(path) = server.lock_file_path.clone() else {
            return;
        };
        let folders = workspace_folders(cx)
            .into_iter()
            .map(|folder| folder.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let contents = serde_json::json!({
            "pid": std::process::id(),
            "workspaceFolders": folders,
            "ideName": "Zed",
            "transport": "ws",
            "runningInWindows": cfg!(windows),
            "authToken": &*server.auth_token,
        })
        .to_string();
        cx.background_spawn(async move { write_lock_file(&path, &contents) })
            .detach_and_log_err(cx);
    }

    fn remove_lock_file(&self) {
        if let Some(path) = self
            .server
            .as_ref()
            .and_then(|server| server.lock_file_path.as_ref())
        {
            std::fs::remove_file(path).ok();
        }
    }

    /// Picks the window to show `abs_path` in: the one whose project contains
    /// it most specifically, else the active one.
    fn workspace_for_path(
        &self,
        abs_path: &Path,
        cx: &mut App,
    ) -> Option<(AnyWindowHandle, Entity<Workspace>)> {
        let app_state = AppState::try_global(cx)?;
        let workspaces = app_state
            .workspace_store
            .read(cx)
            .workspaces_with_windows()
            .filter_map(|(window, workspace)| Some((window, workspace.upgrade()?)))
            .collect::<Vec<_>>();

        let containing = workspaces
            .iter()
            .filter_map(|(window, workspace)| {
                let depth = workspace
                    .read(cx)
                    .root_paths(cx)
                    .iter()
                    .filter(|root| abs_path.starts_with(root))
                    .map(|root| root.components().count())
                    .max()?;
                Some((depth, *window, workspace.clone()))
            })
            .max_by_key(|(depth, _, _)| *depth)
            .map(|(_, window, workspace)| (window, workspace));
        let (window, workspace) = containing.or_else(|| {
            let active_window = cx.active_window();
            workspaces
                .iter()
                .find(|(window, _)| Some(*window) == active_window)
                .or(workspaces.first())
                .cloned()
        })?;

        // A window can hold several projects; show the one we picked.
        window
            .update(cx, |root, window, cx| {
                if let Ok(multi_workspace) = root.downcast::<MultiWorkspace>() {
                    multi_workspace.update(cx, |multi_workspace, cx| {
                        if multi_workspace.workspace() != &workspace {
                            multi_workspace.activate(workspace.clone(), None, window, cx);
                        }
                    });
                }
            })
            .log_err();

        Some((window, workspace))
    }
}

/// Keeps the lock file's folder list in step with the projects that are open.
fn observe_workspace_folders(cx: &mut Context<ClaudeCode>) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        // A project opened from disk has its folders before the workspace exists.
        if let Some(claude_code) = ClaudeCode::global(cx) {
            claude_code.update(cx, |this, cx| this.schedule_lock_file_update(cx));
        }
        let project = workspace.project().clone();
        cx.subscribe(&project, |_, _, event: &project::Event, cx| {
            if matches!(
                event,
                project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_)
            ) && let Some(claude_code) = ClaudeCode::global(cx)
            {
                claude_code.update(cx, |this, cx| this.schedule_lock_file_update(cx));
            }
        })
        .detach();
        cx.on_release(|_, cx| {
            if let Some(claude_code) = ClaudeCode::global(cx) {
                claude_code.update(cx, |this, cx| this.schedule_lock_file_update(cx));
            }
        })
        .detach();
    })
    .detach();
}

fn workspace_folders(cx: &App) -> Vec<Arc<Path>> {
    let Some(app_state) = AppState::try_global(cx) else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    for workspace in app_state.workspace_store.read(cx).workspaces() {
        let Some(workspace) = workspace.upgrade() else {
            continue;
        };
        let workspace = workspace.read(cx);
        if !workspace.project().read(cx).is_local() {
            continue;
        }
        for root in workspace.root_paths(cx) {
            if !folders.contains(&root) {
                folders.push(root);
            }
        }
    }
    folders
}

/// Where the CLI looks for IDEs to connect to.
fn lock_file_dir() -> PathBuf {
    let config_dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => paths::home_dir().join(".claude"),
    };
    config_dir.join("ide")
}

fn write_lock_file(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write as _;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // The file holds the token that authorizes connections.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(contents.as_bytes())?;
    Ok(())
}

/// The URI form the CLI uses for files: `file://` and the path, unescaped.
pub(crate) fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

pub(crate) fn path_from_uri(uri: &str) -> PathBuf {
    PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri))
}

/// The pane `panes` keeps for `workspace`, split off the active one the first
/// time and again whenever the user has closed it.
pub(crate) fn dedicated_pane(
    panes: &mut Vec<(WeakEntity<Workspace>, WeakEntity<Pane>)>,
    workspace: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Pane> {
    panes.retain(|(workspace, pane)| workspace.upgrade().is_some() && pane.upgrade().is_some());
    let existing = panes
        .iter()
        .find(|(pane_workspace, _)| pane_workspace == &workspace.downgrade())
        .and_then(|(_, pane)| pane.upgrade())
        .filter(|pane| workspace.read(cx).panes().contains(pane));
    if let Some(pane) = existing {
        return pane;
    }

    let pane = workspace.update(cx, |workspace, cx| {
        let active_pane = workspace.active_pane().clone();
        workspace.split_pane(active_pane, SplitDirection::Right, window, cx)
    });
    panes.retain(|(pane_workspace, _)| pane_workspace != &workspace.downgrade());
    panes.push((workspace.downgrade(), pane.downgrade()));
    pane
}

/// Opens a file in an editor in `pane`, or the active pane, including a file
/// that doesn't exist yet, which Claude is about to create.
pub(crate) fn open_file(
    workspace: &mut Workspace,
    abs_path: PathBuf,
    pane: Option<WeakEntity<Pane>>,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Box<dyn ItemHandle>>> {
    if let Some(project_path) = workspace.project().read(cx).find_project_path(&abs_path, cx) {
        return workspace.open_path(project_path, pane, focus, window, cx);
    }
    let options = OpenOptions {
        visible: Some(OpenVisible::None),
        focus: Some(focus),
        ..Default::default()
    };
    let open = workspace.open_paths(vec![abs_path.clone()], options, pane, window, cx);
    cx.spawn(async move |_, _| {
        open.await
            .into_iter()
            .next()
            .flatten()
            .with_context(|| format!("couldn't open {}", abs_path.display()))?
    })
}
