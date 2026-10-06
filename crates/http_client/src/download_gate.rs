use std::{
    collections::{BTreeMap, HashSet},
    mem,
    sync::Arc,
};

use futures::channel::{mpsc, oneshot};
use gpui_shared_string::SharedString;
use parking_lot::Mutex;

pub const NODE_RUNTIME: &str = "Node.js";
pub const PRETTIER: &str = "Prettier";
pub const COPILOT: &str = "GitHub Copilot";

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BinaryDownload {
    pub tool: SharedString,
}

impl BinaryDownload {
    pub fn new(tool: impl Into<SharedString>) -> Self {
        Self { tool: tool.into() }
    }
}

impl std::fmt::Display for BinaryDownload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.tool)
    }
}

#[derive(Clone)]
pub struct DownloadGate(Arc<Mutex<GateState>>);

impl DownloadGate {
    pub fn new(
        allow_all: bool,
        allowed: impl IntoIterator<Item = BinaryDownload>,
    ) -> (Self, mpsc::UnboundedReceiver<()>) {
        let (changed, changes) = mpsc::unbounded();
        let gate = Self(Arc::new(Mutex::new(GateState {
            allow_all,
            allowed: allowed.into_iter().collect(),
            denied: HashSet::default(),
            pending: BTreeMap::new(),
            changed,
        })));
        (gate, changes)
    }

    pub fn allow_all() -> Self {
        Self::new(true, []).0
    }

    pub fn deny_all() -> Self {
        Self::new(false, []).0
    }

    pub fn is_allowed(&self, download: &BinaryDownload) -> bool {
        self.0.lock().is_allowed(download)
    }

    pub fn check(&self, download: BinaryDownload) -> bool {
        self.register(download, None)
    }

    pub async fn request(&self, download: BinaryDownload) -> bool {
        let (sender, receiver) = oneshot::channel();
        if self.register(download, Some(sender)) {
            return true;
        }
        receiver.await.is_ok()
    }

    pub async fn allowed(&self, download: BinaryDownload, installed: bool) -> bool {
        if installed {
            self.check(download)
        } else {
            self.request(download).await
        }
    }

    pub fn pending(&self) -> Vec<BinaryDownload> {
        self.0.lock().pending.keys().cloned().collect()
    }

    pub fn allow(&self, download: BinaryDownload) {
        let mut state = self.0.lock();
        let waiters = state.pending.remove(&download);
        state.denied.remove(&download);
        state.allowed.insert(download);
        if let Some(waiters) = waiters {
            for waiter in waiters {
                waiter.send(()).ok();
            }
            state.changed.unbounded_send(()).ok();
        }
    }

    pub fn deny(&self, download: BinaryDownload) {
        let mut state = self.0.lock();
        let had_waiters = state.pending.remove(&download).is_some();
        state.denied.insert(download);
        if had_waiters {
            state.changed.unbounded_send(()).ok();
        }
    }

    pub fn clear_allowed(&self) {
        self.0.lock().allowed.clear();
    }

    pub fn set_allow_all(&self, allow_all: bool) {
        let mut state = self.0.lock();
        if state.allow_all == allow_all {
            return;
        }
        state.allow_all = allow_all;
        if allow_all && !state.pending.is_empty() {
            for waiter in mem::take(&mut state.pending).into_values().flatten() {
                waiter.send(()).ok();
            }
            state.changed.unbounded_send(()).ok();
        }
    }

    fn register(&self, download: BinaryDownload, waiter: Option<oneshot::Sender<()>>) -> bool {
        let mut state = self.0.lock();
        if state.is_allowed(&download) {
            return true;
        }
        if state.denied.contains(&download) || state.changed.is_closed() {
            return false;
        }
        let is_new = !state.pending.contains_key(&download);
        let waiters = state.pending.entry(download).or_default();
        waiters.retain(|waiter| !waiter.is_canceled());
        waiters.extend(waiter);
        if is_new {
            state.changed.unbounded_send(()).ok();
        }
        false
    }
}

struct GateState {
    allow_all: bool,
    allowed: HashSet<BinaryDownload>,
    denied: HashSet<BinaryDownload>,
    pending: BTreeMap<BinaryDownload, Vec<oneshot::Sender<()>>>,
    changed: mpsc::UnboundedSender<()>,
}

impl GateState {
    fn is_allowed(&self, download: &BinaryDownload) -> bool {
        self.allow_all || self.allowed.contains(download)
    }
}
