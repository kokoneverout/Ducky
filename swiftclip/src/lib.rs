mod core;
mod diagnostics;
mod model;
mod platform;
mod storage;
mod sync;
#[cfg(test)]
mod tests;
mod ui;
use anyhow::{Context, Result};
use ely_desktop::{TrayIcon, TrayItem, TrayLook};
use futures::StreamExt;
use gpui::*;
use model::*;
use std::{path::PathBuf, time::Duration};
struct Services {
    _tray: Option<TrayIcon>,
    _instance: platform::Instance,
}
impl Global for Services {}
pub fn launch(cx: &mut App, hidden: bool) -> Result<()> {
    let Some(instance) = platform::Instance::acquire()? else {
        return Ok(());
    };
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("无法确定本地数据目录")?
        .join("Ducky").join("clipboard");
    std::fs::create_dir_all(&dir).context("数据目录不可写，请使用 --data-dir 指定目录")?;
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
    let (events, mut receiver) = futures::channel::mpsc::unbounded();
    let native = platform::Native::start(backend.clone(), events.clone(), &settings)?;
    core::start(
        backend.clone(),
        rx,
        events.clone(),
        db,
        settings.clone(),
        native.clone(),
    )?;
    {
            // The host initializes GPUI and the shared Ely theme once.

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1120.), px(760.)),
                    cx,
                ))),
                window_min_size: Some(size(px(860.), px(600.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Ducky · 剪贴板".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let b = backend.clone();
            let s = settings.clone();
            let window = match cx.open_window(options, |window, cx| {
                ui::style_titlebar(window);
                cx.new(|cx| ui::Shell::new(b, s, window, cx))
            }) {
                Ok(w) => w,
                Err(e) => {
                    platform::error_dialog(&format!("窗口创建失败：{e:#}"));
                    return Err(e.into());
                }
            };
            let tray = make_tray(
                window,
                backend.clone(),
                settings.privacy,
                settings.sync_enabled,
                cx,
            );
            let tray = match tray {
                Ok(t) => Some(t),
                Err(e) => {
                    let _ = events.unbounded_send(UiEvent::Error(format!(
                        "托盘不可用，窗口将保持打开：{e:#}"
                    )));
                    None
                }
            };
            let tray_ready = tray.is_some();
            let _ = window.update(cx, |shell, w, cx| {
                shell.tray_ready = tray_ready;
                let weak = cx.entity().downgrade();
                w.on_window_should_close(cx, move |w, cx| {
                    weak.update(cx, |shell, cx| {
                        if shell.settings.minimize_to_tray && shell.tray_ready {
                            shell.hide(w, cx);
                            false
                        } else {
                            shell.backend.send(Command::Shutdown);
                            cx.quit();
                            true
                        }
                    })
                    .unwrap_or(true)
                });
                if hidden && tray_ready {
                    shell.hide(w, cx);
                } else {
                    w.focus(&shell.focus, cx);
                }
            });
            cx.set_global(Services {
                _tray: tray,
                _instance: instance,
            });
            let tray_backend = backend.clone();
            let mut tray_flags = (settings.privacy, settings.sync_enabled);
            cx.spawn(async move |cx| {
                while let Some(event) = receiver.next().await {
                    let rebuild = match &event {
                        UiEvent::Snapshot(s) => {
                            let flags = (s.settings.privacy, s.settings.sync_enabled);
                            let changed = flags != tray_flags;
                            tray_flags = flags;
                            changed
                        }
                        UiEvent::TrayRestart => true,
                        _ => false,
                    };
                    if window
                        .update(cx, |shell, w, cx| shell.receive(event, w, cx))
                        .is_err()
                    {
                        break;
                    }
                    if rebuild {
                        let b = tray_backend.clone();
                        cx.update(|cx| {
                            match make_tray(window, b, tray_flags.0, tray_flags.1, cx) {
                                Ok(tray) => {
                                    cx.global_mut::<Services>()._tray = Some(tray);
                                }
                                Err(e) => {
                                    let _ = window.update(cx, |shell, w, cx| {
                                        shell.receive(
                                            UiEvent::Error(format!("托盘恢复失败：{e:#}")),
                                            w,
                                            cx,
                                        )
                                    });
                                }
                            }
                        });
                    }
                }
            })
            .detach();
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
            if !hidden {
                cx.activate(true);
            }
    }
    Ok(())
}

fn make_tray(
    window: WindowHandle<ui::Shell>,
    backend: Backend,
    privacy: bool,
    syncing: bool,
    cx: &mut App,
) -> Result<TrayIcon> {
    let tray_backend = backend;
    TrayIcon::new(
        TrayLook {
            symbol: "doc.on.clipboard",
            png: include_bytes!("../assets/tray.png"),
            name: "Ducky · 剪贴板".into(),
        },
        vec![
            TrayItem::Action("打开历史记录".into()),
            TrayItem::Action(
                if privacy {
                    "恢复捕获与同步"
                } else {
                    "暂停捕获与同步"
                }
                .into(),
            ),
            TrayItem::Action(
                if syncing {
                    "关闭自动同步"
                } else {
                    "启用自动同步"
                }
                .into(),
            ),
            TrayItem::Action("立即同步".into()),
            TrayItem::Action("设置".into()),
            TrayItem::Separator,
            TrayItem::Action("退出".into()),
        ],
        move |i, cx| {
            let b = tray_backend.clone();
            let _ = window.update(cx, move |shell, w, cx| match i {
                0 => shell.summon(0, w, cx),
                1 => {
                    let mut s = shell.settings.clone();
                    s.privacy = !s.privacy;
                    b.send(Command::Configure(s));
                }
                2 => {
                    let mut s = shell.settings.clone();
                    s.sync_enabled = !s.sync_enabled;
                    b.send(Command::Configure(s));
                }
                3 => b.send(Command::SyncNow),
                4 => shell.receive(UiEvent::OpenSettings, w, cx),
                6 => shell.receive(UiEvent::Quit, w, cx),
                _ => {}
            });
        },
        cx,
    )
}
