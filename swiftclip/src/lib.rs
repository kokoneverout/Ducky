mod core;
mod diagnostics;
pub mod model;
mod platform;
mod storage;
mod sync;
#[cfg(test)]
mod tests;
pub mod ui;

use anyhow::{Context, Result};
use model::{Backend, Command, Settings, UiEvent};
use std::{path::PathBuf, time::Duration};

/// Clipboard services embedded in the host's existing window and event loop.
pub struct Service {
    pub backend: Backend,
    pub settings: Settings,
    pub receiver: futures::channel::mpsc::UnboundedReceiver<UiEvent>,
    _instance: platform::Instance,
}

impl Service {
    pub fn start() -> Result<Self> {
        let instance = platform::Instance::acquire()?
            .context("独立版 SwiftClip 正在运行，请先退出后再启动 Ducky")?;
        let dir = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .context("无法确定本地数据目录")?
            .join("Ducky")
            .join("clipboard");
        std::fs::create_dir_all(&dir).context("剪贴板数据目录不可写")?;
        let dir = std::fs::canonicalize(dir)?;
        diagnostics::init(&dir.join("logs"))?;
        let settings = Settings::load(&dir)?;
        let db = storage::Database::open(&dir.join("history.sqlite3"))?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let backend = Backend {
            done: Default::default(),
            clipboard_budget: Default::default(),
            tx,
            data_dir: dir,
        };
        let (events, receiver) = futures::channel::mpsc::unbounded();
        let native = platform::Native::start(backend.clone(), events.clone(), &settings)?;
        core::start(backend.clone(), rx, events, db, settings.clone(), native)?;
        Ok(Self {
            backend,
            settings,
            receiver,
            _instance: instance,
        })
    }

    pub fn install_shutdown(&self, cx: &mut gpui::App) {
        let backend = self.backend.clone();
        cx.on_app_quit(move |cx| {
            backend.send(Command::Shutdown);
            let done = backend.done.clone();
            let executor = cx.background_executor().clone();
            async move {
                let start = std::time::Instant::now();
                while !done.load(std::sync::atomic::Ordering::Acquire)
                    && start.elapsed() < Duration::from_secs(2)
                {
                    executor.timer(Duration::from_millis(20)).await;
                }
            }
        })
        .detach();
    }
}
