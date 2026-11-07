use crate::{model::*, platform::Native, storage::Database, sync};
use anyhow::{Context, Result};
use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};
pub fn start(
    backend: Backend,
    rx: mpsc::UnboundedReceiver<Command>,
    ui: UiSender,
    db: Database,
    s: Settings,
    native: Native,
) -> Result<()> {
    thread::Builder::new()
        .name("swiftclip-sync".into())
        .spawn(move || {
            let done = backend.done.clone();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build();
            match runtime {
                Ok(rt) => {
                    if let Err(e) = rt.block_on(run(backend, rx, ui.clone(), db, s, native)) {
                        let _ = ui.unbounded_send(UiEvent::Error(format!("{e:#}")));
                    }
                }
                Err(e) => {
                    let _ = ui.unbounded_send(UiEvent::Error(e.to_string()));
                }
            }
            done.store(true, std::sync::atomic::Ordering::Release);
        })?;
    Ok(())
}
struct Engine {
    backend: Backend,
    ui: UiSender,
    db: Database,
    s: Settings,
    password: String,
    dir: PathBuf,
    native: Native,
    client: reqwest::Client,
    epoch: u64,
    generation: u64,
    pending: Option<(u64, String)>,
    observed: Option<String>,
    remote: Option<sync::Remote>,
    applied: Option<String>,
    conflict: bool,
    flight: Option<JoinHandle<()>>,
    flight_seq: u32,
    needs_read: bool,
    due: Instant,
    last_text: Option<String>,
    last_sync: Option<i64>,
    error: Option<String>,
    status: String,
    query: (u64, String, bool, usize),
    undo: Option<(Instant, Record)>,
    initial_record: bool,
    remote_saved: Option<String>,
    retry: u32,
    test_id: u64,
}
impl Engine {
    fn emit(&self, e: UiEvent) {
        let _ = self.ui.unbounded_send(e);
    }
    fn snapshot(&self) {
        self.emit(UiEvent::Snapshot(Snapshot {
            settings: self.s.clone(),
            status: if self.s.privacy {
                "隐私暂停".into()
            } else if !self.s.sync_enabled {
                "同步已关闭".into()
            } else if self.s.server_url.is_empty() {
                "未配置服务器".into()
            } else {
                self.status.clone()
            },
            error: self.error.clone(),
            last_sync: self.last_sync,
        }));
    }
    fn refresh(&self) {
        if self.query.3 == 0 {
            return;
        }
        let db = self.db.clone();
        let ui = self.ui.clone();
        let (revision, q, pinned, limit) = self.query.clone();
        tokio::spawn(async move {
            match db.query(q, pinned, limit).await {
                Ok((rows, total, matched)) => {
                    let _ = ui.unbounded_send(UiEvent::History {
                        revision,
                        rows,
                        total,
                        matched,
                    });
                }
                Err(e) => {
                    let _ = ui.unbounded_send(UiEvent::Error(format!("历史读取失败：{e:#}")));
                }
            }
        });
    }
    fn reset(&mut self) {
        self.epoch += 1;
        if let Some(h) = self.flight.take() {
            h.abort();
        }
        self.observed = None;
        self.applied = None;
        self.remote_saved = None;
        self.pending = None;
        self.conflict = false;
        self.needs_read = true;
        self.retry = 0;
        self.error = None;
        self.status = "连接中".into();
        self.due = Instant::now();
    }
    async fn save(&mut self, text: &str, source: &str, pin: bool) -> Result<()> {
        if self.s.history_enabled && text.len() <= self.s.max_text_bytes {
            self.db
                .add(text.into(), source, pin, self.s.clone())
                .await?;
            self.refresh();
        } else if pin {
            anyhow::bail!("未能持久保存冲突副本，请选择使用本地或远程内容");
        }
        Ok(())
    }
    async fn local(&mut self, text: String, initial: bool) -> Result<()> {
        if !self.s.listening() {
            return Ok(());
        }
        self.generation += 1;
        let different = self
            .last_text
            .as_deref()
            .is_none_or(|last| key(last) != key(&text));
        self.last_text = Some(text.clone());
        if initial {
            if self.initial_record && different {
                self.save(&text, "local", false).await?;
            }
            self.initial_record = false;
            return Ok(());
        }
        if !different {
            return Ok(());
        }
        // A newer text must invalidate an old unsent candidate even if the new text cannot sync.
        self.pending = None;
        self.save(&text, "local", false)
            .await
            .context("保存新文本失败，未自动上传")?;
        if self.s.syncing() && text.len() <= self.s.max_sync_bytes {
            self.pending = Some((self.generation, text));
            self.due = Instant::now() + Duration::from_millis(200);
        } else if text.len() > self.s.max_text_bytes {
            self.emit(UiEvent::Notice("文本超过上限，未保存".into()));
        }
        self.snapshot();
        Ok(())
    }
    async fn copy(&mut self, id: i64, target: Option<PasteTarget>) -> Result<()> {
        crate::diagnostics::record("copy_read_record", serde_json::json!({"record_id":id}));
        let record = self.db.record(id).await?;
        let sequence = self.native.write(record.content.clone(), None).await?;
        crate::diagnostics::record("copy_write_ack", serde_json::json!({"record_id":id}));
        self.db.copied(id).await?;
        self.last_text = Some(record.content.clone());
        self.generation += 1;
        self.pending = None;
        if self.s.syncing() && record.content.len() <= self.s.max_sync_bytes {
            self.pending = Some((self.generation, record.content));
            self.due = Instant::now() + Duration::from_millis(200);
        }
        self.emit(UiEvent::Copied { target, sequence });
        Ok(())
    }
    async fn apply(&mut self, r: sync::Remote, expected: u32) -> Result<()> {
        if let Some(text) = r.text.as_ref() {
            if self.remote_saved.as_ref() != Some(&r.signature) {
                self.save(text, "remote", false)
                    .await
                    .context("保存远端文本失败，暂停写入剪贴板")?;
                self.remote_saved = Some(r.signature.clone());
            }
            self.native.write(text.clone(), Some(expected)).await?;
            self.last_text = Some(text.clone());
            self.applied = Some(r.signature);
            self.last_sync = Some(now());
        }
        Ok(())
    }
    async fn received(&mut self, r: sync::Remote, start_gen: u64) -> Result<()> {
        self.retry = 0;
        self.error = None;
        self.status = "已连接".into();
        let first = self.observed.is_none();
        let changed = self.observed.as_ref() != Some(&r.signature);
        self.remote = Some(r.clone());
        if first {
            self.observed = Some(r.signature);
            self.needs_read = false;
            self.due = Instant::now();
            return Ok(());
        }
        if let Some((_, text)) = self.pending.as_ref() {
            if r.signature == format!("Text:{}", key(text)) {
                self.pending = None;
                self.last_sync = Some(now());
                self.needs_read = false;
            } else if changed {
                if let Some(remote_text) = r.text.as_ref() {
                    if let Err(e) = self.save(remote_text, "remote", true).await {
                        self.conflict = true;
                        self.status = "同步冲突".into();
                        self.error = Some(e.to_string());
                    } else {
                        self.emit(UiEvent::Notice(
                            "远端冲突副本已自动收藏保留；将上传本地最新内容".into(),
                        ));
                        self.needs_read = false;
                    }
                } else {
                    self.conflict = true;
                    self.status = "同步冲突".into();
                    self.error = Some("远端为非文本，上传本地会替换它；请选择".into());
                }
            } else {
                self.needs_read = false;
            }
        } else if changed
            || (self.applied.as_ref() != Some(&r.signature)
                && self.remote_saved.as_ref() == Some(&r.signature))
        {
            if start_gen == self.generation && crate::platform::sequence() == self.flight_seq {
                self.apply(r.clone(), self.flight_seq).await?;
            } else {
                // Save the observed remote value, but never apply a response to a newer clipboard generation.
                if let Some(t) = r.text.as_ref() {
                    self.save(t, "remote", true).await?;
                }
            }
        }
        self.observed = Some(r.signature);
        self.due = Instant::now() + Duration::from_secs(self.s.poll_seconds);
        Ok(())
    }
    fn launch(&mut self) {
        if !self.s.syncing() || self.flight.is_some() || self.conflict || Instant::now() < self.due
        {
            return;
        }
        let tx = self.backend.tx.clone();
        let c = self.client.clone();
        let s = self.s.clone();
        let password = self.password.clone();
        let epoch = self.epoch;
        let generation = self.generation;
        self.flight_seq = crate::platform::sequence();
        let upload = if self.observed.is_some() && !self.needs_read {
            self.pending.clone()
        } else {
            None
        };
        self.status = if upload.is_some() {
            "正在上传"
        } else {
            "正在检查"
        }
        .into();
        self.flight = Some(tokio::spawn(async move {
            let result = if let Some((g, text)) = upload {
                sync::upload(&c, &s, &password, &text)
                    .await
                    .map(|_| NetworkResult::Uploaded {
                        generation: g,
                        key: key(&text),
                    })
            } else {
                sync::fetch(&c, &s, &password)
                    .await
                    .map(NetworkResult::Fetched)
            };
            let _ = tx.send(Command::Network {
                epoch,
                generation,
                result: result.map_err(|e| format!("{e:#}")),
            });
        }));
    }
    async fn command(&mut self, cmd: Command) -> Result<bool> {
        match cmd {
            Command::Shutdown => {
                if let Some(h) = self.flight.take() {
                    h.abort();
                }
                self.native.stop();
                return Ok(false);
            }
            Command::Clipboard { seq, text, initial } => {
                let _ = self.backend.clipboard_budget.try_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |n| Some(n.saturating_sub(text.len())),
                );
                if seq != 0 {
                    self.local(text, initial).await?;
                }
            }
            Command::SuspendHistory => {
                self.query.3 = 0;
            }
            Command::Query {
                revision,
                query,
                pinned,
                limit,
            } => {
                self.query = (revision, query, pinned, limit);
                self.refresh();
            }
            Command::Detail(id) => {
                let record = self.db.record(id).await?;
                self.emit(UiEvent::Detail(record));
            }
            Command::Copy { i, target } => self.copy(i, target).await?,
            Command::Upload(id) => {
                anyhow::ensure!(self.s.syncing(), "请先启用同步");
                let r = self.db.record(id).await?;
                anyhow::ensure!(r.content.len() <= self.s.max_sync_bytes, "文本超过同步上限");
                self.generation += 1;
                self.pending = Some((self.generation, r.content));
                self.needs_read = true;
                self.due = Instant::now();
            }

            Command::Pin(id, p) => {
                self.db.pin(id, p).await?;
                self.db.clean(self.s.clone()).await?;
                self.refresh();
            }
            Command::Delete(id) => {
                let r = self.db.record(id).await?;
                self.db.delete(id).await?;
                self.undo = Some((Instant::now(), r));
                self.refresh();
                self.emit(UiEvent::Notice("已删除，可在 5 秒内撤销".into()));
            }
            Command::Undo => {
                if let Some((at, r)) = self.undo.take()
                    && at.elapsed() < Duration::from_secs(5)
                {
                    self.db.restore(r, self.s.clone()).await?;
                    self.refresh();
                }
            }
            Command::Clear(all) => {
                self.db.clear(all).await?;
                self.undo = None;
                self.refresh();
                self.emit(UiEvent::Notice("历史已清空".into()));
            }
            Command::Configure(mut s) => {
                s.server_url = self.s.server_url.clone();
                s.username = self.s.username.clone();
                self.native.configure(&s).await?;
                if s.startup != self.s.startup
                    && let Err(e) = crate::platform::startup(s.startup, &self.dir)
                {
                    let _ = self.native.configure(&self.s).await;
                    return Err(e);
                }
                if let Err(e) = s.save(&self.dir) {
                    let _ = self.native.configure(&self.s).await;
                    if s.startup != self.s.startup {
                        let _ = crate::platform::startup(self.s.startup, &self.dir);
                    }
                    return Err(e);
                }
                let reset = s.sync_enabled != self.s.sync_enabled
                    || s.privacy != self.s.privacy
                    || s.max_sync_bytes != self.s.max_sync_bytes;
                let baseline =
                    (!self.s.listening() && s.listening()) || (self.s.privacy && !s.privacy);
                self.s = s;
                if reset {
                    self.reset();
                }
                if self.s.privacy {
                    self.undo = None;
                }
                if baseline {
                    self.initial_record = false;
                    self.native.baseline();
                }
                self.db.clean(self.s.clone()).await?;
                self.refresh();
                self.snapshot();
            }
            Command::Connect {
                url,
                user,
                password,
            } => {
                anyhow::ensure!(!self.s.privacy, "隐私暂停期间不会发出连接请求");
                let url = sync::base_url(&url)?
                    .to_string()
                    .trim_end_matches('/')
                    .to_string();
                self.test_id += 1;
                let test_id = self.test_id;
                let mut s = self.s.clone();
                s.server_url = url;
                s.username = user;
                let c = self.client.clone();
                let tx = self.backend.tx.clone();
                tokio::spawn(async move {
                    let result =
                        sync::fetch(&c, &s, &password)
                            .await
                            .map(|_| NetworkResult::Tested {
                                settings: s,
                                password,
                            });
                    let _ = tx.send(Command::Network {
                        epoch: test_id,
                        generation: u64::MAX,
                        result: result.map_err(|e| format!("{e:#}")),
                    });
                });
                self.emit(UiEvent::Notice("正在只读测试连接…".into()));
            }
            Command::SyncNow => {
                if self.s.syncing() && !self.conflict {
                    self.due = Instant::now();
                    self.needs_read = true;
                }
            }
            Command::Resolve(local) => {
                anyhow::ensure!(self.s.syncing(), "请先启用同步并退出隐私暂停");
                if let Some(r) = self.remote.clone() {
                    if local {
                        self.conflict = false;
                        self.needs_read = false;
                        self.observed = Some(r.signature);
                        self.due = Instant::now();
                        self.error = None;
                    } else {
                        self.pending = None;
                        self.apply(r.clone(), crate::platform::sequence()).await?;
                        self.conflict = false;
                        self.error = None;
                        self.observed = Some(r.signature);
                        self.needs_read = false;
                    }
                }
                self.snapshot();
            }
            Command::Network {
                epoch,
                generation,
                result,
            } => {
                if generation == u64::MAX {
                    if epoch != self.test_id {
                        return Ok(true);
                    }
                    match result {
                        Ok(NetworkResult::Tested {
                            settings: tested,
                            password,
                        }) => {
                            // Keep non-connection settings changed during the read-only test.
                            let mut s = self.s.clone();
                            s.server_url = tested.server_url;
                            s.username = tested.username;
                            crate::platform::save_password(&s, &password)?;
                            s.save(&self.dir)?;
                            self.s = s;
                            self.password = password;
                            self.reset();
                            self.emit(UiEvent::Notice(
                                "连接测试成功，配置已应用（写入权限将在上传时验证）".into(),
                            ));
                            self.snapshot();
                        }
                        Err(e) => {
                            self.emit(UiEvent::Error(format!("连接测试失败，原配置保持有效：{e}")));
                        }
                        _ => {}
                    }
                    return Ok(true);
                }
                if epoch != self.epoch {
                    return Ok(true);
                }
                self.flight = None;
                match result {
                    Ok(NetworkResult::Fetched(r)) => self.received(r, generation).await?,
                    Ok(NetworkResult::Uploaded {
                        generation: g,
                        key: k,
                    }) => {
                        self.observed = Some(format!("Text:{k}"));
                        if self.pending.as_ref().is_some_and(|(pg, _)| *pg == g) {
                            self.pending = None;
                        }
                        self.last_sync = Some(now());
                        self.retry = 0;
                        self.error = None;
                        self.status = "已连接".into();
                        self.due = if self.pending.is_some() {
                            Instant::now()
                        } else {
                            Instant::now() + Duration::from_secs(self.s.poll_seconds)
                        };
                    }
                    Err(e) => {
                        self.retry = self.retry.saturating_add(1);
                        let delay = if e.contains("401") || e.contains("403") {
                            300
                        } else {
                            (2u64.pow(self.retry.min(5))).min(30)
                        };
                        self.needs_read = true;
                        self.due = Instant::now() + Duration::from_secs(delay);
                        self.status = "等待重试".into();
                        self.error = Some(e);
                    }
                    _ => {}
                }
                self.snapshot();
            }
        }
        Ok(true)
    }
}
async fn run(
    backend: Backend,
    mut rx: mpsc::UnboundedReceiver<Command>,
    ui: UiSender,
    db: Database,
    s: Settings,
    native: Native,
) -> Result<()> {
    let mut s = s;
    let (password, credential_error) = match crate::platform::password(&s) {
        Ok(p) => (p, None),
        Err(e) => {
            s.sync_enabled = false;
            (
                String::new(),
                Some(format!("凭据读取失败，本地历史仍可使用：{e:#}")),
            )
        }
    };
    let last_text = db.latest().await?;
    let mut e = Engine {
        dir: backend.data_dir.clone(),
        backend,
        ui,
        db,
        s,
        password,
        native,
        client: sync::client()?,
        epoch: 1,
        generation: 0,
        pending: None,
        observed: None,
        remote: None,
        applied: None,
        conflict: false,
        flight: None,
        flight_seq: 0,
        needs_read: true,
        due: Instant::now(),
        last_text,
        last_sync: None,
        error: credential_error,
        status: "连接中".into(),
        query: (0, String::new(), false, 100),
        undo: None,
        initial_record: true,
        remote_saved: None,
        retry: 0,
        test_id: 0,
    };
    e.snapshot();
    e.native.baseline();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
         cmd=rx.recv()=>{let Some(cmd)=cmd else{break;};match e.command(cmd).await{Ok(true)=>{},Ok(false)=>break,Err(err)=>{e.error=Some(format!("{err:#}"));e.status=if e.conflict{"同步冲突"}else{"需要处理"}.into();e.snapshot();e.emit(UiEvent::Error(format!("{err:#}")));e.due=Instant::now()+Duration::from_secs(5);}}},
         _=tick.tick()=>{e.launch();}
        }
    }
    Ok(())
}

#[cfg(test)]
mod scenario_tests {
    use super::*;
    fn engine(s: Settings) -> Engine {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "swiftclip-core-{}-{}-{}",
            std::process::id(),
            now(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(&dir.join("history.sqlite3")).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let (ui, _) = futures::channel::mpsc::unbounded();
        Engine {
            backend: Backend {
                done: Default::default(),
                clipboard_budget: Default::default(),
                tx,
                data_dir: dir.clone(),
            },
            ui,
            db,
            s,
            password: String::new(),
            dir,
            native: Native::test_stub(),
            client: sync::client().unwrap(),
            epoch: 1,
            generation: 0,
            pending: None,
            observed: None,
            remote: None,
            applied: None,
            conflict: false,
            flight: None,
            flight_seq: 0,
            needs_read: true,
            due: Instant::now(),
            last_text: None,
            last_sync: None,
            error: None,
            status: String::new(),
            query: (0, String::new(), false, 100),
            undo: None,
            initial_record: true,
            remote_saved: None,
            retry: 0,
            test_id: 0,
        }
    }
    fn remote(text: &str) -> sync::Remote {
        sync::Remote {
            signature: format!("Text:{}", key(text)),
            text: Some(text.into()),
        }
    }
    fn settings() -> Settings {
        Settings {
            server_url: "http://127.0.0.1:1".into(),
            sync_enabled: true,
            ..Default::default()
        }
    }
    #[tokio::test]
    async fn startup_only_establishes_baseline() {
        let mut e = engine(settings());
        e.local("startup".into(), true).await.unwrap();
        assert!(e.pending.is_none());
        e.received(remote("server"), 0).await.unwrap();
        assert!(e.applied.is_none());
        assert!(e.pending.is_none());
        assert_eq!(e.db.query(String::new(), false, 50).await.unwrap().1, 1);
    }
    #[tokio::test]
    async fn an_old_upload_ack_never_discards_newer_local_text() {
        let mut e = engine(settings());
        e.observed = Some(remote("old").signature);
        e.local("A".into(), false).await.unwrap();
        let a = e.generation;
        e.local("B".into(), false).await.unwrap();
        let b = e.generation;
        e.command(Command::Network {
            epoch: e.epoch,
            generation: a,
            result: Ok(NetworkResult::Uploaded {
                generation: a,
                key: key("A"),
            }),
        })
        .await
        .unwrap();
        assert_eq!(e.pending, Some((b, "B".into())));
        let epoch = e.epoch;
        e.reset();
        e.pending = Some((b + 1, "C".into()));
        e.command(Command::Network {
            epoch,
            generation: b + 1,
            result: Ok(NetworkResult::Uploaded {
                generation: b + 1,
                key: key("C"),
            }),
        })
        .await
        .unwrap();
        assert_eq!(e.pending, Some((b + 1, "C".into())));
    }
    #[tokio::test]
    async fn observed_conflict_is_pinned_before_local_can_overwrite() {
        let mut s = settings();
        s.history_limit = 1;
        let mut e = engine(s);
        e.observed = Some(remote("baseline").signature);
        e.local("local".into(), false).await.unwrap();
        e.received(remote("new-remote"), e.generation)
            .await
            .unwrap();
        assert!(!e.conflict);
        assert!(!e.needs_read);
        let (rows, _, _) = e.db.query("new-remote".into(), false, 50).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].pinned);
        assert!(e.pending.is_some());
    }
    #[tokio::test]
    async fn history_disabled_conflict_cannot_be_bypassed_by_sync_now() {
        let mut s = settings();
        s.history_enabled = false;
        let mut e = engine(s);
        e.observed = Some(remote("baseline").signature);
        e.local("local".into(), false).await.unwrap();
        e.received(remote("new-remote"), e.generation)
            .await
            .unwrap();
        assert!(e.conflict);
        e.command(Command::SyncNow).await.unwrap();
        assert!(e.conflict);
        assert!(e.applied.is_none());
    }
    #[tokio::test]
    async fn privacy_recovery_does_not_capture_or_replay_paused_text() {
        let mut s = settings();
        s.privacy = true;
        let mut e = engine(s.clone());
        e.pending = Some((3, "old".into()));
        s.privacy = false;
        e.command(Command::Configure(s)).await.unwrap();
        e.local("paused-content".into(), true).await.unwrap();
        assert!(e.pending.is_none());
        assert_eq!(e.db.query(String::new(), false, 50).await.unwrap().1, 0);
    }
}
