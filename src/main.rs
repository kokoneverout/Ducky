#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod assets;
mod audio;
mod config;
mod desktop;
mod engine;
mod history;
mod overlay;
mod platform;
mod startup;
mod ui;

use futures_lite::StreamExt;
use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use std::sync::{Arc, RwLock};

fn main() {
    if let Err(error) = run()
        && !std::env::args().any(|arg| arg == "--background")
    {
        platform::show_error(&format!("{error:#}"));
    }
}

fn run() -> anyhow::Result<()> {
    let smoke = std::env::args().any(|arg| arg == "--smoke-ui");
    let preview =
        std::env::args().any(|arg| matches!(arg.as_str(), "--preview-ui" | "--preview-api"));
    let isolated = smoke || preview;
    let background = std::env::args().any(|arg| arg == "--background");
    let _instance = if isolated {
        None
    } else {
        Some(platform::Instance::acquire()?)
    };
    let config = Arc::new(RwLock::new(if isolated {
        config::Config::isolated(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/ui-smoke-config.json"),
        )
    } else {
        config::Config::load()?
    }));
    let history = if isolated {
        history::History::at(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/ui-smoke-history.json"),
        )?
    } else {
        history::History::load()?
    };
    let (events, receive) = async_channel::unbounded();
    let startup = if isolated {
        None
    } else {
        engine::start(config.clone(), events.clone())
            .err()
            .map(|error| format!("{error:#}"))
    };
    let _tray = desktop::Tray::start(events.clone())?;
    let popup = overlay::Overlay::start(events.clone())?;
    gpui_platform::application()
        .with_assets(ely_core::Assets::before(assets::Assets))
        .run(move |cx: &mut App| {
            if let Err(error) = ely_core::init(cx) {
                if !background {
                    platform::show_error(&format!("界面初始化失败：{error:#}"));
                }
                cx.quit();
                return;
            }
            ely_core::theme::Theme::update(cx, |theme| {
                theme.font_family = "MiSans".into();
                theme.reduced_motion = true;
            });
            let mut palette = ely_core::theme::Palette::light(false);
            for (name, color) in [
                ("bg", ui::PAPER),
                ("surface", ui::PAPER),
                ("sunken", 0xeee9d8),
                ("overlay", ui::PAPER),
                ("hover", 0xecebdc),
                ("active", 0xe1e4d2),
                ("border", 0xe6e1cf),
                ("border_strong", 0xd6d8c3),
                ("fg", 0x4d584e),
                ("fg_subtle", 0x899181),
                ("accent", 0x75866b),
                ("accent_hover", 0x65775c),
                ("focus", 0x89997b),
            ] {
                *palette.token_mut(name) = gpui::rgb(color).into();
            }
            ely_core::theme::Theme::set_palette(ely_core::theme::Mode::Light, Some(palette), cx);
            let clipboard_service = if isolated {
                None
            } else {
                match swiftclip::Service::start() {
                    Ok(service) => {
                        service.install_shutdown(cx);
                        Some(service)
                    }
                    Err(error) => {
                        platform::show_error(&format!("剪贴板模块启动失败：{error:#}"));
                        None
                    }
                }
            };
            let clipboard = clipboard_service
                .as_ref()
                .map(|service| (service.backend.clone(), service.settings.clone()));
            let bounds = Bounds::centered(None, size(px(900.0), px(680.0)), cx);
            let ui_events = events.clone();
            let ui_config = config.clone();
            let opened = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(if isolated {
                            "Ducky · 界面预览".into()
                        } else {
                            "Ducky".into()
                        }),
                        appears_transparent: false,
                        ..Default::default()
                    }),
                    window_min_size: Some(size(px(720.0), px(500.0))),
                    show: !background,
                    focus: !background,
                    ..Default::default()
                },
                |window, cx| {
                    desktop::prepare_main(window);
                    window.on_window_should_close(cx, |_, _| {
                        desktop::hide_main();
                        false
                    });
                    cx.new(|cx| {
                        ui::VoiceApp::new(
                            ui_config, ui_events, history, startup, isolated, clipboard, window, cx,
                        )
                    })
                },
            );
            let main = match opened {
                Ok(window) => window,
                Err(error) => {
                    if !background {
                        platform::show_error(&format!("打开窗口失败：{error:#}"));
                    }
                    cx.quit();
                    return;
                }
            };
            if let Some(mut service) = clipboard_service {
                cx.spawn(async move |cx| {
                    while let Some(event) = service.receiver.next().await {
                        if main
                            .update(cx, |app, window, cx| app.clipboard_event(event, window, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
            }
            if smoke {
                run_smoke(main, popup.probe(), events.clone(), cx);
            }
            if preview {
                let events = events.clone();
                cx.spawn(async move |cx| {
                    // Start-Process may apply SW_HIDE to the initial ShowWindow.
                    // Reveal the isolated preview after initial window creation.
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(150))
                        .await;
                    desktop::show_main();
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(180))
                        .await;
                    let _ = events.try_send(engine::Event::Quit);
                })
                .detach();
            }
            cx.spawn(async move |cx| {
                while let Ok(event) = receive.recv().await {
                    if matches!(event, engine::Event::Quit) {
                        cx.update(|cx| cx.quit());
                        break;
                    }
                    let animation = match &event {
                        engine::Event::Preview(settings) => settings.clone(),
                        _ => config
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .animation
                            .clone(),
                    };
                    popup.event(&event, &animation);
                    let _ = main.update(cx, |app, _, cx| app.event(event, cx));
                }
            })
            .detach();
            if !background {
                cx.activate(true);
            }
        });
    Ok(())
}

fn run_smoke(
    main: gpui::WindowHandle<ui::VoiceApp>,
    popup: overlay::Probe,
    events: async_channel::Sender<engine::Event>,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(400))
            .await;
        let mut checks = Vec::new();
        let _ = main.update(cx, |app, _, cx| checks.extend(app.smoke_startup(cx)));
        if std::env::args().any(|arg| arg == "--background") {
            let _ = main.update(cx, |_, window, _| {
                checks.push(("background_starts_hidden", !desktop::is_visible(window)))
            });
            checks.push(("background_overlay_hidden", !popup.visible()));
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/background-smoke-report.json");
            let _ = std::fs::write(path, serde_json::to_vec_pretty(&checks).unwrap());
            let _ = events.try_send(engine::Event::Quit);
            return;
        }
        checks.push((
            "misans_available",
            cx.update(|cx| {
                cx.text_system()
                    .all_font_names()
                    .iter()
                    .any(|name| name == "MiSans")
            }),
        ));
        checks.push(("overlay_styles", popup.validate()));
        let _ = main.update(cx, |app, window, _| {
            checks.push(("history_default", app.is_history_page()));
            checks.push(("caption_draggable", desktop::caption_is_draggable(window)));
            desktop::system_command(
                window,
                windows_sys::Win32::UI::WindowsAndMessaging::SC_MAXIMIZE,
            );
        });
        cx.background_executor()
            .timer(std::time::Duration::from_millis(200))
            .await;
        let _ = main.update(cx, |_, window, _| {
            checks.push(("maximized", desktop::maximized(window)));
            desktop::system_command(
                window,
                windows_sys::Win32::UI::WindowsAndMessaging::SC_RESTORE,
            );
        });
        cx.background_executor()
            .timer(std::time::Duration::from_millis(200))
            .await;
        let _ = main.update(cx, |_, window, _| {
            checks.push(("restored", !desktop::maximized(window)));
            desktop::system_command(
                window,
                windows_sys::Win32::UI::WindowsAndMessaging::SC_MINIMIZE,
            );
        });
        cx.background_executor()
            .timer(std::time::Duration::from_millis(200))
            .await;
        let _ = main.update(cx, |_, window, _| {
            checks.push((
                "minimized_in_taskbar",
                desktop::minimized(window) && desktop::is_visible(window),
            ))
        });
        desktop::show_main();
        cx.background_executor()
            .timer(std::time::Duration::from_millis(200))
            .await;
        let _ = main.update(cx, |_, window, _| {
            checks.push(("minimize_restored", !desktop::minimized(window)));
            desktop::request_close(window);
        });
        cx.background_executor()
            .timer(std::time::Duration::from_millis(200))
            .await;
        let _ = main.update(cx, |_, window, _| {
            checks.push(("close_to_tray", !desktop::is_visible(window)))
        });
        desktop::show_main();
        let _ = events.try_send(engine::Event::Preview(config::AnimationConfig::default()));
        cx.background_executor()
            .timer(std::time::Duration::from_millis(400))
            .await;
        checks.push(("overlay_visible", popup.visible()));
        checks.push(("gif_frames_rendered", popup.frames_drawn() > 2));
        desktop::hide_main();
        let _ = main.update(cx, |_, window, _| {
            checks.push(("main_hidden", !desktop::is_visible(window)))
        });
        checks.push(("overlay_survives_hidden_main", popup.visible()));
        desktop::show_main();
        let _ = main.update(cx, |_, window, _| {
            checks.push(("tray_restore", desktop::is_visible(window)))
        });
        cx.background_executor()
            .timer(std::time::Duration::from_secs(3))
            .await;
        checks.push(("preview_hides", !popup.visible()));
        for phase in 0..5 {
            let _ = main.update(cx, |app, _, cx| checks.extend(app.smoke_api(phase, cx)));
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
        }
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/ui-smoke-report.json");
        let _ = std::fs::write(path, serde_json::to_vec_pretty(&checks).unwrap());
        let _ = events.try_send(engine::Event::Quit);
    })
    .detach();
}
