use crate::{model::*, platform};
use ely_core::{
    buttons::{Button, ButtonVariant, IconButton},
    forms::{
        Checkbox, Choice, Input, InputEvent, PasswordInput, SearchInput, Select, Switch, TextInput,
    },
    layout::ScrollArea,
    menus::{ContextMenu, Menu as ElyMenu, MenuItem as ElyMenuItem},
    navigation::NavItem,
    overlays::Dialog,
    primitives::{FocusScope, Icon, IconName},
    theme::{ActiveTheme, ControlSize, Mode, Palette, Theme},
    typography::SelectableText,
};
use gpui::{prelude::*, *};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::time::Duration;

pub fn hwnd(window: &Window) -> isize {
    match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}
const PAGE_BACKGROUND: u32 = 0xfaf5e6;
const PAGE_FOREGROUND: u32 = 0x415451;
const UI_FONT: &str = "MiSans";

pub fn style_titlebar(window: &Window) {
    platform::style_titlebar(hwnd(window), PAGE_BACKGROUND, PAGE_FOREGROUND);
}
pub fn setup_theme(cx: &mut App) {
    let mut light = Palette::light(false);
    light.bg = rgb(PAGE_BACKGROUND).into();
    light.sunken = rgb(0xfcf7e8).into();
    light.surface = rgb(0xfffcf5).into();
    light.overlay = light.surface;
    light.fg = rgb(PAGE_FOREGROUND).into();
    light.fg_muted = rgb(0x6d786d).into();
    light.fg_subtle = light.fg_muted;
    light.border = rgb(0xe5e1d4).into();
    light.accent = rgb(0x3f6256).into();
    light.selection = rgb(0xe6e7da).into();
    light.hover = light.selection;
    Theme::set_palette(Mode::Light, Some(light), cx);
    Theme::set_mode_now(Mode::Light, cx);
    Theme::update(cx, |t| {
        t.font_family = UI_FONT.into();
        t.reduced_motion = true;
    });
}

pub struct Shell {
    pub backend: Backend,
    pub settings: Settings,
    pub focus: FocusHandle,
    scope: FocusHandle,
    loaded_revision: u64,
    copy_inflight: bool,
    pub page: bool,
    pub hidden: bool,
    pub tray_ready: bool,
    pub quick_target: Option<PasteTarget>,
    search: Entity<TextInput>,
    url: Entity<TextInput>,
    user: Entity<TextInput>,
    password: Entity<TextInput>,
    rows: Vec<Row>,
    total: usize,
    matched: usize,
    selected: usize,
    pinned: bool,
    revision: u64,
    limit: usize,
    scroll: UniformListScrollHandle,
    detail: Option<Record>,
    detail_page: usize,
    clear: bool,
    clear_pinned: bool,
    status: String,
    error: Option<String>,
    notice: Option<String>,
    notice_epoch: u64,
    last_sync: Option<i64>,
    new_count: usize,
    _subscriptions: Vec<Subscription>,
    search_task: Option<Task<()>>,
}
impl Shell {
    pub fn new(
        backend: Backend,
        settings: Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            TextInput::new(window, cx)
                .placeholder("搜索剪贴板内容…")
                .label("搜索剪贴板")
        });
        let url =
            cx.new(|cx| TextInput::new(window, cx).placeholder("https://your-server.example/"));
        url.update(cx, |i, cx| i.set_text(settings.server_url.clone(), cx));
        let user = cx.new(|cx| TextInput::new(window, cx).placeholder("用户名"));
        user.update(cx, |i, cx| i.set_text(settings.username.clone(), cx));
        let password = cx.new(|cx| {
            TextInput::new(window, cx)
                .placeholder("输入密码以测试并应用")
                .masked()
        });
        let sub = cx.subscribe_in(&search, window, |this, _, event: &InputEvent, _, cx| {
            if *event == InputEvent::Changed {
                this.revision += 1;
                let revision = this.revision;
                this.search_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(150))
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        if this.revision == revision {
                            this.limit = 100;
                            this.selected = 0;
                            this.query(cx);
                        }
                    });
                }));
            }
        });
        // GPUI updates the native caption mode when Windows appearance changes.
        // Keep our fixed light caption in sync without exposing a theme setting.
        let appearance = cx.observe_window_appearance(window, |_, window, _| {
            style_titlebar(window);
        });
        let weak = cx.entity().downgrade();
        let window_id = hwnd(window);
        let keys = cx.intercept_keystrokes(move |event, window, cx| {
            if hwnd(window) != window_id {
                return;
            }
            let _ = weak.update(cx, |this, cx| {
                this.key(
                    &KeyDownEvent {
                        keystroke: event.keystroke.clone(),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                )
            });
        });
        let shell = Self {
            backend,
            settings,
            focus: cx.focus_handle(),
            scope: cx.focus_handle(),
            loaded_revision: 0,
            copy_inflight: false,
            page: false,
            hidden: false,
            tray_ready: false,
            quick_target: None,
            search,
            url,
            user,
            password,
            rows: Vec::new(),
            total: 0,
            matched: 0,
            selected: 0,
            pinned: false,
            revision: 0,
            limit: 100,
            scroll: UniformListScrollHandle::new(),
            detail: None,
            detail_page: 0,
            clear: false,
            clear_pinned: false,
            status: "正在启动".into(),
            error: None,
            notice: None,
            notice_epoch: 0,
            last_sync: None,
            new_count: 0,
            _subscriptions: vec![sub, keys, appearance],
            search_task: None,
        };
        shell.query(cx);
        shell
    }
    fn query(&self, cx: &App) {
        self.backend.send(Command::Query {
            revision: self.revision,
            query: self.search.read(cx).committed().to_string(),
            pinned: self.pinned,
            limit: self.limit,
        });
    }
    fn save(&self) {
        self.backend.send(Command::Configure(self.settings.clone()));
    }
    pub fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tray_ready {
            self.message("托盘尚未就绪，窗口保持打开".into(), cx);
            return;
        }
        window.focus(&self.focus, cx);
        self.backend.send(Command::SuspendHistory);
        self.hidden = true;
        self.rows.clear();
        self.detail = None;
        platform::hide(hwnd(window));
        cx.notify();
    }
    pub fn summon(&mut self, target: isize, window: &mut Window, cx: &mut Context<Self>) {
        crate::diagnostics::window("summon_target", target);
        if self.copy_inflight {
            crate::diagnostics::record("summon_ignored_paste_busy", serde_json::json!({}));
            return;
        }
        if target == 0 {
            self.quick_target = None;
        } else if target != hwnd(window) {
            self.quick_target = platform::target(target);
        }
        self.copy_inflight = false;
        self.hidden = false;
        self.page = false;
        self.detail = None;
        self.clear = false;
        self.selected = 0;
        self.limit = 100;
        self.new_count = 0;
        self.search.update(cx, |i, cx| i.set_text("", cx));
        self.revision += 1;
        self.query(cx);
        platform::show(hwnd(window));
        window.activate_window();
        window.focus(&self.search.read(cx).focus().clone(), cx);
        cx.notify();
    }
    pub fn receive(&mut self, event: UiEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            UiEvent::Snapshot(s) => {
                self.settings = s.settings;
                self.status = s.status;
                self.error = s.error.or(self.error.take());
                self.last_sync = s.last_sync;
            }
            UiEvent::History {
                revision,
                rows,
                total,
                matched,
            } => {
                self.total = total;
                self.matched = matched;
                if revision == self.revision && !self.hidden && !self.page {
                    if revision == self.loaded_revision
                        && self.scroll.0.borrow().base_handle.offset().y < px(-1.)
                        && rows.first().map(|r| r.id) != self.rows.first().map(|r| r.id)
                        && !self.rows.is_empty()
                    {
                        self.new_count = total.saturating_sub(self.rows.len()).max(1);
                    } else {
                        let id = self.rows.get(self.selected).map(|r| r.id);
                        self.rows = rows;
                        self.loaded_revision = revision;
                        self.selected = id
                            .and_then(|id| self.rows.iter().position(|r| r.id == id))
                            .unwrap_or(0)
                            .min(self.rows.len().saturating_sub(1));
                    }
                }
            }
            UiEvent::Detail(r) => {
                self.detail = Some(r);
                self.detail_page = 0;
            }
            UiEvent::Notice(s) => self.message(s, cx),
            UiEvent::Error(s) => {
                self.copy_inflight = false;
                self.error = Some(s);
                if self.hidden {
                    self.hidden = false;
                    platform::show(hwnd(window));
                }
            }
            UiEvent::Summon(target) => self.summon(target, window, cx),
            UiEvent::OpenSettings => {
                self.hidden = false;
                self.backend.send(Command::SuspendHistory);
                self.page = true;
                self.quick_target = None;
                self.rows.clear();
                platform::show(hwnd(window));
            }
            UiEvent::Copied { target, sequence } => {
                crate::diagnostics::record(
                    "ui_clipboard_confirmed",
                    serde_json::json!({"hidden":self.hidden,"copy_inflight":self.copy_inflight}),
                );
                if let Some(target) = target {
                    if self.hidden || self.page || !self.copy_inflight {
                        self.copy_inflight = false;
                        crate::diagnostics::record(
                            "paste_cancelled_after_dismiss",
                            serde_json::json!({}),
                        );
                        return;
                    }
                    self.hide(window, cx);
                    self.quick_target = None;
                    let own_hwnd = hwnd(window);
                    crate::diagnostics::record("paste_task_scheduled", serde_json::json!({}));
                    cx.spawn(async move |this, cx| {
                        crate::diagnostics::record("paste_task_started",serde_json::json!({}));
                        let result = cx
                            .background_executor()
                            .spawn(
                                async move { platform::paste(target, sequence).map_err(|e| e.to_string()) },
                            )
                            .await;
                        crate::diagnostics::record("paste_task_completed",serde_json::json!({"success":result.is_ok(),"error":result.as_ref().err()}));
                        let _ = this.update(cx, |this, cx| {
                            this.copy_inflight = false;
                            if let Err(e) = result {
                                this.error = Some(e);
                                this.hidden = false;
                                platform::show(own_hwnd);
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                } else {
                    self.message("已复制".into(), cx);
                }
            }
            UiEvent::TrayRestart => {}
            UiEvent::Quit => {
                self.backend.send(Command::Shutdown);
                cx.quit();
            }
        }
        cx.notify();
    }
    fn message(&mut self, s: String, cx: &mut Context<Self>) {
        self.notice = Some(s);
        self.notice_epoch += 1;
        let epoch = self.notice_epoch;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let _ = this.update(cx, |this, cx| {
                if this.notice_epoch == epoch {
                    this.notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }
    fn move_selection(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = if down {
            (self.selected + 1).min(self.rows.len() - 1)
        } else {
            self.selected.saturating_sub(1)
        };
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
        window.focus(&self.focus, cx);
        cx.notify();
    }
    fn confirm(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        if self.copy_inflight || self.loaded_revision != self.revision {
            crate::diagnostics::record(
                "confirm_ignored",
                serde_json::json!({"inflight":self.copy_inflight,"loaded_revision":self.loaded_revision,"revision":self.revision}),
            );
            return;
        }
        crate::diagnostics::begin(self.quick_target);
        if self.quick_target.is_some() {
            self.copy_inflight = true;
        }
        if let Some(r) = self.rows.get(self.selected) {
            self.backend.send(if let Some(target) = self.quick_target {
                Command::Copy {
                    i: r.id,
                    target: Some(target),
                }
            } else {
                Command::Detail(r.id)
            });
        }
    }
    fn key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.page {
            return;
        }
        let k = e.keystroke.key.as_str();
        if self.search.read(cx).edits().1 {
            return;
        }
        if self.detail.is_some() || self.clear {
            if k == "escape" {
                self.detail = None;
                self.clear = false;
                window.focus(&self.focus, cx);
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        match k {
            "down" => {
                self.move_selection(true, window, cx);
                cx.stop_propagation();
            }
            "up" => {
                self.move_selection(false, window, cx);
                cx.stop_propagation();
            }
            "enter" => {
                self.confirm();
                cx.stop_propagation();
            }
            "escape" => {
                if !self.search.read(cx).is_empty() {
                    self.search.update(cx, |i, cx| i.set_text("", cx));
                } else {
                    let target = self.quick_target.take();
                    self.hide(window, cx);
                    if let Some(t) = target {
                        platform::restore_target(t);
                    }
                }
                cx.stop_propagation();
            }
            "f" if e.keystroke.modifiers.control => {
                window.focus(&self.search.read(cx).focus().clone(), cx);
                cx.stop_propagation();
            }
            "c" if e.keystroke.modifiers.control && self.focus.is_focused(window) => {
                if let Some(r) = self.rows.get(self.selected) {
                    self.backend.send(Command::Copy {
                        i: r.id,
                        target: None,
                    });
                }
                cx.stop_propagation();
            }
            "delete" if self.focus.is_focused(window) => {
                if let Some(r) = self.rows.get(self.selected) {
                    self.backend.send(Command::Delete(r.id));
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }
    fn row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let r = self.rows[ix].clone();
        let id = r.id;
        let pinned = r.pinned;
        let colors = cx.theme().colors.clone();
        let preview = r.preview.lines().take(3).collect::<Vec<_>>().join("\n");
        let body = div()
            .id(("row", id as u64))
            .h(px(156.))
            .w_full()
            .py_3()
            .px_3()
            .border_b_1()
            .border_color(colors.border)
            .when(ix == self.selected, |d| d.bg(colors.hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, w, cx| {
                    this.selected = ix;
                    w.focus(&this.focus, cx);
                    cx.notify();
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .mb_3()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.fg_muted)
                            .child(format!(
                                "{}  ·  {}{}",
                                format_time(r.created),
                                if r.source == "remote" {
                                    "远程"
                                } else {
                                    "本地"
                                },
                                if r.pinned { "  ·  已收藏" } else { "" }
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                IconButton::new(("copy", id as u64), IconName::Copy)
                                    .tooltip("复制")
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.backend.send(Command::Copy {
                                            i: id,
                                            target: None,
                                        });
                                    })),
                            )
                            .child(
                                IconButton::new(("pin", id as u64), IconName::Pin)
                                    .tooltip(if pinned {
                                        "取消收藏"
                                    } else {
                                        "收藏置顶"
                                    })
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.backend.send(Command::Pin(id, !pinned));
                                    })),
                            )
                            .child(
                                IconButton::new(("delete", id as u64), IconName::Trash2)
                                    .tooltip("删除")
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.backend.send(Command::Delete(id));
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .h(px(65.))
                    .overflow_hidden()
                    .text_size(px(15.))
                    .line_height(px(22.))
                    .child(preview),
            )
            .child(
                div().flex().child(
                    Button::new(
                        ("detail", id as u64),
                        format!("查看全文 · {} 字符", r.chars),
                    )
                    .variant(ButtonVariant::Link)
                    .size(ControlSize::Sm)
                    .on_click(
                        cx.listener(move |this, _, _, _| this.backend.send(Command::Detail(id))),
                    ),
                ),
            )
            .into_any_element();
        let b1 = self.backend.clone();
        let b2 = self.backend.clone();
        let b3 = self.backend.clone();
        let b4 = self.backend.clone();
        let b5 = self.backend.clone();
        ContextMenu::new(
            ("row-menu", id as u64),
            ElyMenu::new()
                .item(
                    ElyMenuItem::new("复制")
                        .icon(IconName::Copy)
                        .keys("Ctrl+C")
                        .on_click(move |_, _| {
                            b1.send(Command::Copy {
                                i: id,
                                target: None,
                            })
                        }),
                )
                .item(
                    ElyMenuItem::new("查看全文").on_click(move |_, _| b2.send(Command::Detail(id))),
                )
                .item(
                    ElyMenuItem::new(if pinned {
                        "取消收藏"
                    } else {
                        "收藏置顶"
                    })
                    .icon(IconName::Pin)
                    .on_click(move |_, _| b3.send(Command::Pin(id, !pinned))),
                )
                .item(
                    ElyMenuItem::new("上传此条")
                        .disabled(!self.settings.syncing())
                        .on_click(move |_, _| b4.send(Command::Upload(id))),
                )
                .separator()
                .item(
                    ElyMenuItem::new("删除")
                        .icon(IconName::Trash2)
                        .on_click(move |_, _| b5.send(Command::Delete(id))),
                ),
        )
        .h(px(156.))
        .child(body)
        .into_any_element()
    }
    fn history(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_start()
                    .mb_6()
                    .child(
                        div()
                            .child(
                                div()
                                    .text_size(px(28.))
                                    .font_weight(FontWeight::BOLD)
                                    .child("历史记录"),
                            )
                            .child(
                                div()
                                    .mt_2()
                                    .text_size(px(13.))
                                    .text_color(colors.fg_muted)
                                    .child(if self.search.read(cx).is_empty() && !self.pinned {
                                        format!("{} 条记录", self.total)
                                    } else {
                                        format!("找到 {} 条 / 共 {} 条", self.matched, self.total)
                                    }),
                            ),
                    )
                    .child(
                        Button::new("clear-history", "清空历史")
                            .variant(ButtonVariant::Link)
                            .disabled(self.total == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clear = true;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_5()
                    .mb_5()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(SearchInput::new("search", &self.search)),
                    )
                    .child(
                        Checkbox::new("pinned-only", self.pinned)
                            .label("仅收藏")
                            .on_change({
                                let weak = cx.entity().downgrade();
                                move |on, _, cx| {
                                    let _ = weak.update(cx, |this, cx| {
                                        this.pinned = on;
                                        this.selected = 0;
                                        this.limit = 100;
                                        this.revision += 1;
                                        this.query(cx);
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
            )
            .when(self.quick_target.is_some(), |d| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.accent)
                        .mb_3()
                        .child("快速上屏   ↑ ↓ 选择 · Enter 粘贴 · Esc 返回原应用"),
                )
            })
            .when(self.new_count > 0, |d| {
                d.child(
                    Button::new("new-items", "有新记录，点击刷新").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.new_count = 0;
                            this.rows.clear();
                            this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                            this.query(cx);
                        },
                    )),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .when(self.rows.is_empty(), |d| {
                        d.flex().items_center().justify_center().child(
                            div().text_color(colors.fg_muted).child(
                                if self.matched == 0
                                    && (!self.search.read(cx).is_empty() || self.pinned)
                                {
                                    "没有匹配的记录"
                                } else {
                                    "复制一段文字，SwiftClip 会记录在这里"
                                },
                            ),
                        )
                    })
                    .when(!self.rows.is_empty(), |d| {
                        d.child(
                            uniform_list(
                                "history-list",
                                self.rows.len(),
                                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                    range.map(|ix| this.row(ix, cx)).collect::<Vec<_>>()
                                }),
                            )
                            .track_scroll(&self.scroll)
                            .size_full(),
                        )
                    }),
            )
            .when(self.rows.len() < self.matched, |d| {
                d.child(
                    Button::new("load-more", "加载更多")
                        .variant(ButtonVariant::Ghost)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.limit += 100;
                            this.query(cx);
                        })),
                )
            })
            .into_any_element()
    }
    fn setting_switch(
        &self,
        id: &'static str,
        label: &'static str,
        value: bool,
        apply: fn(&mut Settings, bool),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.entity().downgrade();
        div()
            .flex()
            .items_center()
            .justify_between()
            .py_3()
            .child(label)
            .child(
                Switch::new(id, value)
                    .name(label)
                    .on_change(move |v, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            apply(&mut this.settings, v);
                            this.save();
                            cx.notify();
                        });
                    }),
            )
            .into_any_element()
    }
    fn setting_select(
        &self,
        id: &'static str,
        label: &'static str,
        value: String,
        choices: Vec<(&'static str, &'static str)>,
        apply: fn(&mut Settings, &str),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.entity().downgrade();
        div()
            .flex()
            .items_center()
            .justify_between()
            .py_3()
            .child(label)
            .child(
                Select::new(id, choices.into_iter().map(|(v, l)| Choice::new(v, l)))
                    .label(label)
                    .selected(value)
                    .on_change(move |v, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            apply(&mut this.settings, v.as_ref());
                            this.save();
                            cx.notify();
                        });
                    }),
            )
            .into_any_element()
    }
    fn settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.theme().colors.clone();
        let heading = |s: &'static str| {
            div()
                .mt_8()
                .mb_3()
                .pb_2()
                .border_b_1()
                .border_color(c.border)
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(18.))
                .child(s)
        };
        ScrollArea::new("settings-scroll")
            .size_full()
            .child(
                div()
                    .pb_8()
                    .child(
                        div()
                            .text_size(px(28.))
                            .font_weight(FontWeight::BOLD)
                            .child("设置"),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_color(c.fg_muted)
                            .text_size(px(13.))
                            .child("本地记录，安静同步。"),
                    )
                    .child(heading("连接"))
                    .child(div().mb_3().text_size(px(13.)).child("服务器地址"))
                    .child(Input::new(&self.url))
                    .child(div().my_3().text_size(px(13.)).child("用户名"))
                    .child(Input::new(&self.user))
                    .child(div().my_3().text_size(px(13.)).child("密码"))
                    .child(PasswordInput::new(&self.password))
                    .child(
                        div().mt_4().child(
                            Button::new("test-connect", "测试并应用")
                                .primary()
                                .disabled(self.settings.privacy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.backend.send(Command::Connect {
                                        url: this.url.read(cx).text().to_string(),
                                        user: this.user.read(cx).text().to_string(),
                                        password: this.password.read(cx).text().to_string(),
                                    });
                                })),
                        ),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_size(px(12.))
                            .text_color(c.fg_muted)
                            .child("测试仅执行读取，不改动远端剪贴板。HTTP 会明文传输凭据。"),
                    )
                    .child(heading("同步"))
                    .child(self.setting_switch(
                        "sync-enabled",
                        "自动双向同步",
                        self.settings.sync_enabled,
                        |s, v| s.sync_enabled = v,
                        cx,
                    ))
                    .child(self.setting_select(
                        "poll",
                        "轮询间隔",
                        self.settings.poll_seconds.to_string(),
                        vec![("1", "1 秒"), ("2", "2 秒"), ("5", "5 秒"), ("10", "10 秒")],
                        |s, v| s.poll_seconds = v.parse().unwrap_or(2),
                        cx,
                    ))
                    .child(self.setting_select(
                        "sync-size",
                        "单条同步上限",
                        self.settings.max_sync_bytes.to_string(),
                        vec![
                            ("65536", "64 KiB"),
                            ("1048576", "1 MiB"),
                            ("4194304", "4 MiB"),
                            ("16777216", "16 MiB"),
                        ],
                        |s, v| s.max_sync_bytes = v.parse().unwrap_or(1048576),
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(
                                Button::new("sync-now", "立即同步")
                                    .disabled(!self.settings.syncing())
                                    .on_click(cx.listener(|this, _, _, _| {
                                        this.backend.send(Command::SyncNow)
                                    })),
                            )
                            .child(
                                Button::new("use-remote", "使用当前远端")
                                    .disabled(!self.settings.syncing())
                                    .on_click(cx.listener(|this, _, _, _| {
                                        this.backend.send(Command::Resolve(false))
                                    })),
                            ),
                    )
                    .when(self.status == "同步冲突", |d| {
                        d.child(div().mt_3().child(
                            Button::new("keep-local", "确认上传本地内容").on_click(cx.listener(
                                |this, _, _, _| this.backend.send(Command::Resolve(true)),
                            )),
                        ))
                    })
                    .child(heading("历史记录"))
                    .child(self.setting_switch(
                        "history-enabled",
                        "保存本地历史",
                        self.settings.history_enabled,
                        |s, v| s.history_enabled = v,
                        cx,
                    ))
                    .child(self.setting_select(
                        "history-count",
                        "最大记录数量",
                        self.settings.history_limit.to_string(),
                        vec![
                            ("100", "100 条"),
                            ("500", "500 条"),
                            ("1000", "1000 条"),
                            ("5000", "5000 条"),
                            ("0", "不限"),
                        ],
                        |s, v| s.history_limit = v.parse().unwrap_or(1000),
                        cx,
                    ))
                    .child(self.setting_select(
                        "history-size",
                        "单条保存上限",
                        self.settings.max_text_bytes.to_string(),
                        vec![
                            ("65536", "64 KiB"),
                            ("1048576", "1 MiB"),
                            ("4194304", "4 MiB"),
                            ("16777216", "16 MiB"),
                        ],
                        |s, v| s.max_text_bytes = v.parse().unwrap_or(1048576),
                        cx,
                    ))
                    .child(self.setting_select(
                        "retention",
                        "自动清理期限",
                        self.settings.retention_days.to_string(),
                        vec![
                            ("0", "不限"),
                            ("7", "7 天"),
                            ("30", "30 天"),
                            ("90", "90 天"),
                        ],
                        |s, v| s.retention_days = v.parse().unwrap_or(0),
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(c.fg_muted)
                            .child("收藏不参与自动清理。历史关闭时仍可同步，但冲突需要手动选择。"),
                    )
                    .child(heading("常规"))
                    .child(self.setting_switch(
                        "right-alt",
                        "右 Alt 单独按下呼出",
                        self.settings.right_alt,
                        |s, v| s.right_alt = v,
                        cx,
                    ))
                    .child(div().text_size(px(12.)).text_color(c.fg_muted).child(
                        "↑ ↓ 选择条目，Enter 直接粘贴。组合键保留原行为；备用快捷键 Ctrl+Alt+H。",
                    ))
                    .child(self.setting_select(
                        "hotkey",
                        "备用呼出快捷键",
                        self.settings.hotkey.clone(),
                        vec![
                            ("Ctrl+Alt+H", "Ctrl+Alt+H"),
                            ("Ctrl+Alt+J", "Ctrl+Alt+J"),
                            ("Ctrl+Shift+H", "Ctrl+Shift+H"),
                            ("Ctrl+Shift+J", "Ctrl+Shift+J"),
                        ],
                        |s, v| s.hotkey = v.into(),
                        cx,
                    ))
                    .child(self.setting_switch(
                        "startup",
                        "开机启动到托盘",
                        self.settings.startup,
                        |s, v| s.startup = v,
                        cx,
                    ))
                    .child(self.setting_switch(
                        "minimize",
                        "关闭窗口时收起到托盘",
                        self.settings.minimize_to_tray,
                        |s, v| s.minimize_to_tray = v,
                        cx,
                    ))
                    .child(heading("隐私"))
                    .child(self.setting_switch(
                        "privacy",
                        "暂停捕获与所有同步",
                        self.settings.privacy,
                        |s, v| s.privacy = v,
                        cx,
                    ))
                    .child(div().text_size(px(12.)).text_color(c.fg_muted).child(
                        "历史以完整文本存于本地，默认未加密。密码保存在本机 Windows 凭据管理器。",
                    ))
                    .child(heading("关于"))
                    .child(div().text_size(px(13.)).child(concat!(
                        "SwiftClip ",
                        env!("CARGO_PKG_VERSION"),
                        " · Rust / GPUI / Ely"
                    )))
                    .child(
                        div()
                            .mt_2()
                            .w_full()
                            .min_w_0()
                            .whitespace_normal()
                            .text_size(px(12.))
                            .text_color(c.fg_muted)
                            .child(format!(
                                "数据目录：{}",
                                platform::display_path(&self.backend.data_dir)
                            )),
                    ),
            )
            .into_any_element()
    }
    fn detail_view(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let record = self.detail.as_ref()?;
        let id = record.row.id;
        let weak = cx.entity().downgrade();
        let chunks = (record.content.chars().count().saturating_sub(1) / 16000) + 1;
        let text = record
            .content
            .chars()
            .skip(self.detail_page * 16000)
            .take(16000)
            .collect::<String>();
        Some(
            Dialog::new("detail", "完整文本", move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.detail = None;
                    cx.notify();
                });
            })
            .detail(format!(
                "{} · {} 字符 · {} 字节 · 第 {} / {} 段",
                format_time(record.row.created),
                record.row.chars,
                record.row.bytes,
                self.detail_page + 1,
                chunks
            ))
            .child(
                ScrollArea::new(("detail-body", self.detail_page))
                    .h(px(360.))
                    .child(SelectableText::new(("content", self.detail_page), text)),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .mt_3()
                    .child(
                        Button::new("previous-part", "上一段")
                            .disabled(self.detail_page == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.detail_page = this.detail_page.saturating_sub(1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("copy-detail", "复制全文")
                            .primary()
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.backend.send(Command::Copy {
                                    i: id,
                                    target: None,
                                })
                            })),
                    )
                    .child(
                        Button::new("next-part", "下一段")
                            .disabled(self.detail_page + 1 >= chunks)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.detail_page += 1;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element(),
        )
    }
}
impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let detail = self.detail_view(cx);
        let side = div()
            .flex_none()
            .w(px(224.))
            .h_full()
            .bg(colors.sunken)
            .border_r_1()
            .border_color(colors.border)
            .p_5()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .pt_4()
                    .pb_10()
                    .child(
                        div()
                            .size(px(38.))
                            .rounded(px(12.))
                            .bg(colors.accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::Clipboard).color(colors.on_accent)),
                    )
                    .child(
                        div()
                            .text_size(px(24.))
                            .font_weight(FontWeight::BOLD)
                            .child("SwiftClip"),
                    ),
            )
            .child(
                NavItem::new("nav-history", IconName::History, "历史记录")
                    .active(!self.page)
                    .on_click({
                        let weak = cx.entity().downgrade();
                        move |w, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.page = false;
                                this.revision += 1;
                                this.query(cx);
                                w.focus(&this.focus, cx);
                                cx.notify();
                            });
                        }
                    }),
            )
            .child(
                NavItem::new("nav-settings", IconName::Settings, "设置")
                    .active(self.page)
                    .on_click({
                        let weak = cx.entity().downgrade();
                        move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.backend.send(Command::SuspendHistory);
                                this.page = true;
                                this.quick_target = None;
                                this.rows.clear();
                                cx.notify();
                            });
                        }
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(colors.fg_muted)
                    .mb_2()
                    .child(if self.settings.privacy {
                        "Ⅱ 隐私暂停"
                    } else if self.settings.history_enabled {
                        "● 正在记录"
                    } else {
                        "○ 历史未启用"
                    }),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(if self.error.is_some() {
                        colors.danger
                    } else {
                        colors.fg_muted
                    })
                    .mb_2()
                    .child(self.status.clone()),
            )
            .when_some(self.last_sync, |d, at| {
                d.child(
                    div()
                        .text_size(px(11.))
                        .text_color(colors.fg_muted)
                        .mb_3()
                        .child(format!("最近同步 {}", format_time(at))),
                )
            })
            .child(
                Button::new("hide-window", "收起到托盘")
                    .variant(ButtonVariant::Ghost)
                    .on_click(cx.listener(|this, _, w, cx| this.hide(w, cx))),
            );
        let content = if self.page {
            self.settings_page(cx)
        } else {
            self.history(cx)
        };
        let root = div()
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .font_family(UI_FONT)
            .text_size(px(15.))
            .bg(colors.bg)
            .text_color(colors.fg)
            .child(side)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .p_8()
                    .flex()
                    .flex_col()
                    .when_some(self.error.clone(), |d, error| {
                        d.child(
                            div()
                                .mb_4()
                                .p_3()
                                .bg(colors.danger_subtle)
                                .rounded_md()
                                .text_size(px(12.))
                                .text_color(colors.danger)
                                .child(error)
                                .child(
                                    Button::new("dismiss-error", "关闭提示")
                                        .variant(ButtonVariant::Link)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.error = None;
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    .when_some(self.notice.clone(), |d, notice| {
                        d.child(
                            div()
                                .mb_3()
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_size(px(12.))
                                .text_color(colors.accent)
                                .child(notice)
                                .child(
                                    Button::new("undo", "撤销删除")
                                        .variant(ButtonVariant::Link)
                                        .size(ControlSize::Sm)
                                        .on_click(cx.listener(|this, _, _, _| {
                                            this.backend.send(Command::Undo)
                                        })),
                                ),
                        )
                    })
                    .child(content),
            )
            .children(detail);
        let root = if self.clear {
            let weak = cx.entity().downgrade();
            root.child(Dialog::new("confirm-clear","清空历史",move|_,cx|{let _=weak.update(cx,|this,cx|{this.clear=false;cx.notify();});})
    .detail(format!("当前共 {} 条记录。默认保留收藏。仅删除本地历史，不清空剪贴板或服务器，此操作不可撤销。",self.total))
    .child(Checkbox::new("clear-pins",self.clear_pinned).label("同时删除收藏").on_change({let weak=cx.entity().downgrade();move|v,_,cx|{let _=weak.update(cx,|this,cx|{this.clear_pinned=v;cx.notify();});}}))
    .child(div().flex().justify_end().mt_4().child(Button::new("confirm-delete-all","确认清空").variant(ButtonVariant::Danger).on_click(cx.listener(|this,_,_,cx|{this.backend.send(Command::Clear(this.clear_pinned));this.clear=false;cx.notify();})))))
        } else {
            root
        };
        FocusScope::new(&self.scope).root().size_full().child(root)
    }
}
fn format_time(ms: i64) -> String {
    let dt = time::OffsetDateTime::from_unix_timestamp(ms / 1000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let offset = time::UtcOffset::current_local_offset()
        .unwrap_or(time::UtcOffset::from_hms(8, 0, 0).unwrap());
    let dt = dt.to_offset(offset);
    format!(
        "{:02}月{:02}日 {:02}:{:02}",
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute()
    )
}
