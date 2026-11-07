use futures::channel::mpsc::UnboundedSender;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedSender as CommandSender;

pub fn hash(text: &str) -> String {
    format!("{:X}", Sha256::digest(text.as_bytes()))
}
pub fn key(text: &str) -> String {
    hash(&text.replace("\r\n", "\n"))
}
pub fn now() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server_url: String,
    pub username: String,
    pub sync_enabled: bool,
    pub poll_seconds: u64,
    pub history_enabled: bool,
    pub history_limit: usize,
    pub max_text_bytes: usize,
    pub max_sync_bytes: usize,
    pub retention_days: u64,
    pub privacy: bool,
    pub minimize_to_tray: bool,
    pub right_alt: bool,
    pub hotkey: String,
    pub startup: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            username: String::new(),
            sync_enabled: false,
            poll_seconds: 2,
            history_enabled: true,
            history_limit: 1000,
            max_text_bytes: 1048576,
            max_sync_bytes: 1048576,
            retention_days: 0,
            privacy: false,
            minimize_to_tray: true,
            right_alt: true,
            hotkey: "Ctrl+Alt+H".into(),
            startup: false,
        }
    }
}
impl Settings {
    pub fn listening(&self) -> bool {
        !self.privacy && (self.history_enabled || self.sync_enabled)
    }
    pub fn syncing(&self) -> bool {
        self.sync_enabled && !self.privacy && !self.server_url.is_empty()
    }
    pub fn load(dir: &std::path::Path) -> anyhow::Result<Self> {
        let p = dir.join("settings.json");
        if !p.exists() {
            return Ok(Self::default());
        }
        let s: Self = serde_json::from_slice(&std::fs::read(p)?)?;
        anyhow::ensure!([1, 2, 5, 10].contains(&s.poll_seconds), "同步间隔无效");
        anyhow::ensure!(
            s.max_text_bytes >= 1024
                && s.max_sync_bytes >= 1024
                && s.max_text_bytes <= 16 * 1048576
                && s.max_sync_bytes <= 16 * 1048576,
            "文本上限应在 1 KiB 至 16 MiB"
        );
        Ok(s)
    }
    pub fn save(&self, dir: &std::path::Path) -> anyhow::Result<()> {
        let tmp = dir.join("settings.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, dir.join("settings.json"))?;
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct Row {
    pub id: i64,
    pub preview: String,
    pub source: String,
    pub created: i64,
    pub pinned: bool,
    pub chars: usize,
    pub bytes: usize,
}
#[derive(Clone, Debug)]
pub struct Record {
    pub row: Row,
    pub content: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncHealth {
    Disconnected,
    Healthy,
    Unhealthy,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub settings: Settings,
    pub status: String,
    pub error: Option<String>,
    pub last_sync: Option<i64>,
}

impl Snapshot {
    pub fn health(&self) -> SyncHealth {
        if !self.settings.syncing() {
            SyncHealth::Disconnected
        } else if self.error.is_some() {
            SyncHealth::Unhealthy
        } else if self.status == "已连接" {
            SyncHealth::Healthy
        } else {
            SyncHealth::Disconnected
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct PasteTarget {
    pub hwnd: isize,
    pub process: u32,
    pub thread: u32,
}
#[derive(Debug)]
pub enum UiEvent {
    Snapshot(Snapshot),
    History {
        revision: u64,
        rows: Vec<Row>,
        total: usize,
        matched: usize,
    },
    Detail(Record),
    Notice(String),
    Error(String),
    Summon(isize),
    OpenSettings,
    TrayRestart,
    Copied {
        target: Option<PasteTarget>,
        sequence: u32,
    },
    Quit,
}
pub type UiSender = UnboundedSender<UiEvent>;
#[derive(Debug)]
pub enum Command {
    Clipboard {
        seq: u32,
        text: String,
        initial: bool,
    },
    Query {
        revision: u64,
        query: String,
        pinned: bool,
        limit: usize,
    },
    Copy {
        i: i64,
        target: Option<PasteTarget>,
    },
    Upload(i64),
    Detail(i64),
    Pin(i64, bool),
    Delete(i64),
    Undo,
    Clear(bool),
    Configure(Settings),
    Connect {
        url: String,
        user: String,
        password: String,
    },
    SuspendHistory,
    SyncNow,
    Resolve(bool),
    Network {
        epoch: u64,
        generation: u64,
        result: Result<NetworkResult, String>,
    },
    Shutdown,
}
#[derive(Debug)]
pub enum NetworkResult {
    Fetched(crate::sync::Remote),
    Uploaded {
        generation: u64,
        key: String,
    },
    Tested {
        settings: Settings,
        password: String,
    },
}
#[derive(Clone)]
pub struct Backend {
    pub done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub clipboard_budget: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub tx: CommandSender<Command>,
    pub data_dir: PathBuf,
}
impl Backend {
    pub fn clipboard(&self, seq: u32, text: String, initial: bool) -> bool {
        use std::sync::atomic::Ordering;
        let len = text.len();
        if self
            .clipboard_budget
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(len).filter(|v| *v <= 32 * 1048576)
            })
            .is_err()
        {
            return false;
        }
        if self
            .tx
            .send(Command::Clipboard { seq, text, initial })
            .is_err()
        {
            self.clipboard_budget.fetch_sub(len, Ordering::Relaxed);
            return false;
        }
        true
    }
    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }
}
