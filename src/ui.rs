mod api_page;
use crate::{
    config::{
        AnimationConfig, Config, DEFAULT_POLISH_PROMPT, DEFAULT_TRANSCRIPTION_PROMPT,
        RecognitionMode,
    },
    desktop,
    engine::Event,
    history::{Entry, History},
    platform,
};
use api_page::ApiState;
use ely_core::{
    buttons::{Button, ButtonVariant, IconButton},
    forms::{Choice, Input, Select, Switch, TextInput},
    layout::{ScrollArea, Scrollbar},
    overlays::Dialog,
    primitives::{FocusScope, Icon, IconName, Tooltip},
    theme::{ControlSize, IconSize},
    typography::SelectableText,
};
use gpui::{prelude::*, *};
use std::sync::{Arc, RwLock};

pub const PAPER: u32 = 0xfbf5e2;
const SIDEBAR: u32 = PAPER;
const INK: u32 = 0x4d584e;
const MUTED: u32 = 0x899181;
const LINE: u32 = 0xe7e3d0;
const SELECTED: u32 = 0xe5e5d5;

#[derive(Clone, Copy, PartialEq)]
enum Page {
    History,
    Api,
    Animation,
    ClipboardHistory,
    ClipboardSettings,
}

pub struct VoiceApp {
    shared: Arc<RwLock<Config>>,
    events: async_channel::Sender<Event>,
    history: History,
    page: Page,
    focus: FocusHandle,
    status: String,
    error: bool,
    working: bool,
    expanded: Option<u64>,
    history_scroll: UniformListScrollHandle,
    confirm_clear: bool,
    unsaved_result: Option<String>,
    mimo_key: Entity<TextInput>,
    mimo_model: Entity<TextInput>,
    recognition_mode: RecognitionMode,
    api: ApiState,
    gif_path: Entity<TextInput>,
    transcription_prompt: Entity<TextInput>,
    polish_enabled: bool,
    polish_url: Entity<TextInput>,
    polish_key: Entity<TextInput>,
    polish_model: Entity<TextInput>,
    polish_prompt: Entity<TextInput>,
    animation: AnimationConfig,
    startup_enabled: bool,
    isolated: bool,
    clipboard: Option<Entity<swiftclip::ui::Shell>>,
}

fn field(
    window: &mut Window,
    cx: &mut App,
    label: &'static str,
    value: &str,
    secret: bool,
    multi: bool,
) -> Entity<TextInput> {
    cx.new(|cx| {
        let mut input = TextInput::new(window, cx).label(label);
        if secret {
            input = input.masked();
        }
        if multi {
            input = input.multi_line(3, 8);
        }
        input.set_text(value, cx);
        input
    })
}

impl VoiceApp {
    pub fn is_history_page(&self) -> bool {
        self.page == Page::History
    }
    pub fn new(
        shared: Arc<RwLock<Config>>,
        events: async_channel::Sender<Event>,
        history: History,
        startup: Option<String>,
        isolated: bool,
        clipboard: Option<(swiftclip::model::Backend, swiftclip::model::Settings)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = shared.read().unwrap_or_else(|e| e.into_inner()).clone();
        let enabled = if isolated {
            Ok(false)
        } else {
            crate::startup::enabled()
        };
        let startup_error = enabled.as_ref().err().map(|error| format!("{error:#}"));
        let startup = startup.or(startup_error);
        let clipboard = clipboard.map(|(backend, settings)| {
            cx.new(|cx| swiftclip::ui::Shell::new(backend, settings, window, cx))
        });
        let mut result = Self {
            clipboard,
            focus: cx.focus_handle(),
            page: Page::History,
            status: startup.clone().unwrap_or_default(),
            error: startup.is_some(),
            working: false,
            history,
            expanded: None,
            history_scroll: UniformListScrollHandle::new(),
            confirm_clear: false,
            unsaved_result: None,
            mimo_key: field(window, cx, "MiMo API Key", &config.mimo_key, true, false),
            mimo_model: field(window, cx, "识别模型", &config.mimo_model, false, false),
            recognition_mode: config.recognition_mode,
            api: ApiState::new(&config, window, cx),
            gif_path: field(
                window,
                cx,
                "GIF 文件",
                &config.animation.gif_path,
                false,
                false,
            ),
            transcription_prompt: field(
                window,
                cx,
                "识别提示词",
                &config.transcription_prompt,
                false,
                true,
            ),
            polish_enabled: config.polish_enabled,
            polish_url: field(window, cx, "润色接口地址", &config.polish_url, false, false),
            polish_key: field(window, cx, "润色 API Key", &config.polish_key, true, false),
            polish_model: field(window, cx, "润色模型", &config.polish_model, false, false),
            polish_prompt: field(window, cx, "润色提示词", &config.polish_prompt, false, true),
            animation: config.animation.clone(),
            startup_enabled: enabled.unwrap_or(false),
            isolated,
            shared,
            events,
        };
        if let Some(clipboard) = &result.clipboard {
            cx.observe(clipboard, |_, _, cx| cx.notify()).detach();
        }
        for input in result.api_inputs() {
            cx.observe(&input, |_, _, cx| cx.notify()).detach();
        }
        if std::env::args().any(|arg| arg == "--preview-api") {
            result.page = Page::Api;
        }
        result
    }

    pub fn event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Recording => {
                self.working = true;
                self.error = false;
                self.status = "录音中".into();
            }
            Event::Recognizing => self.status = "识别中".into(),
            Event::Polishing => self.status = "润色中".into(),
            Event::Notice(message) => {
                self.working = false;
                self.error = false;
                self.status = message;
            }
            Event::Error(message) => {
                self.working = false;
                self.error = true;
                self.status = message;
            }
            Event::AnimationError(message) => {
                self.error = true;
                self.status = message;
            }
            Event::Finished {
                raw,
                text,
                model,
                status,
            } => {
                self.working = false;
                self.error = false;
                self.status = status;
                match self.history.append(raw, text.clone(), model) {
                    Ok(()) => self.unsaved_result = None,
                    Err(error) => {
                        self.unsaved_result = Some(text);
                        self.error = true;
                        self.status = format!("{error:#}；本次结果仍可复制");
                    }
                }
            }
            Event::Preview(_) | Event::Quit => {}
        }
        cx.notify();
    }

    pub fn clipboard_event(
        &mut self,
        event: swiftclip::model::UiEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use swiftclip::model::UiEvent;
        match &event {
            UiEvent::Summon(_) => self.page = Page::ClipboardHistory,
            UiEvent::OpenSettings => self.page = Page::ClipboardSettings,
            _ => {}
        }
        if let Some(clipboard) = &self.clipboard {
            clipboard.update(cx, |shell, cx| shell.receive(event, window, cx));
        }
        cx.notify();
    }

    fn set_startup(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let result = if self.isolated {
            Ok(())
        } else {
            crate::startup::set_enabled(enabled)
        };
        if result.is_ok() {
            self.startup_enabled = enabled;
        }
        self.report(
            result,
            if enabled {
                "已启用开机静默启动"
            } else {
                "已关闭开机静默启动"
            },
            cx,
        );
    }

    pub fn smoke_startup(&mut self, cx: &mut Context<Self>) -> Vec<(&'static str, bool)> {
        assert!(self.isolated, "startup smoke must use an isolated preview");
        self.set_startup(true, cx);
        let enabled = self.startup_enabled;
        self.set_startup(false, cx);
        let disabled = !self.startup_enabled;
        self.status.clear();
        vec![
            ("startup_switch_enabled", enabled),
            ("startup_switch_disabled", disabled),
        ]
    }

    fn save_animation(&mut self, cx: &mut Context<Self>) {
        self.animation.gif_path = self.gif_path.read(cx).text().trim().into();
        let mut config = self
            .shared
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        config.animation = self.animation.clone();
        let result = config.save_preferences();
        if result.is_ok() {
            *self.shared.write().unwrap_or_else(|e| e.into_inner()) = config;
        }
        self.report(result, "已保存", cx);
    }

    fn report(&mut self, result: anyhow::Result<()>, success: &str, cx: &mut Context<Self>) {
        self.error = result.is_err();
        self.status = match result {
            Ok(()) => success.into(),
            Err(error) => format!("{error:#}"),
        };
        cx.notify();
    }

    fn nav(
        &self,
        page: Page,
        label: &'static str,
        icon: IconName,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (health_color, health_label) = match self
            .clipboard
            .as_ref()
            .map(|shell| shell.read(cx).sync_health())
        {
            Some(swiftclip::model::SyncHealth::Healthy) => (0x758f6a, "同步服务健康"),
            Some(swiftclip::model::SyncHealth::Unhealthy) => (0xb45c50, "同步服务异常"),
            _ => (0xa9afa1, "未连接或隐私暂停"),
        };
        div()
            .id(("nav", page as usize))
            .role(Role::Button)
            .aria_label(label)
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py(px(11.0))
            .rounded_md()
            .cursor_pointer()
            .bg(rgb(if self.page == page { SELECTED } else { SIDEBAR }))
            .hover(|s| s.bg(rgb(SELECTED)))
            .child(Icon::new(icon).size(IconSize::Sm).color(rgb(INK)))
            .child(label)
            .when(page == Page::ClipboardSettings, |el| {
                el.child(div().flex_1()).child(
                    div()
                        .id("sync-health")
                        .size(px(8.0))
                        .rounded_full()
                        .bg(rgb(health_color))
                        .aria_label(health_label)
                        .tooltip(Tooltip::text(health_label)),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page = page;
                if let Some(clipboard) = &this.clipboard {
                    clipboard.update(cx, |shell, cx| {
                        shell.navigate(
                            matches!(page, Page::ClipboardHistory | Page::ClipboardSettings),
                            page == Page::ClipboardSettings,
                            cx,
                        )
                    });
                }
                this.confirm_clear = false;
                this.error = false;
                this.status.clear();
                cx.notify();
            }))
    }

    fn history_row(&self, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let id = entry.id;
        let flattened = entry.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = if flattened.chars().count() > 120 {
            format!("{}…", flattened.chars().take(120).collect::<String>())
        } else {
            flattened
        };
        let date = chrono::DateTime::from_timestamp(entry.timestamp, 0)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%m月%d日  %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        let copied = entry.text.clone();
        div()
            .id(("entry", id))
            .group("entry")
            .h(px(106.0))
            .w_full()
            .min_w_0()
            .py_3()
            .border_b_1()
            .border_color(rgb(LINE))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(date)
                            .when(entry.raw != entry.text, |el| el.child("已润色")),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Button::new(("detail", id), "查看全文")
                                    .variant(ButtonVariant::Ghost)
                                    .size(ControlSize::Sm)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.expanded = Some(id);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                IconButton::new(("copy", id), IconName::Copy)
                                    .size(ControlSize::Sm)
                                    .tooltip("复制")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let result = platform::copy(&copied);
                                        this.report(result, "已复制", cx);
                                    })),
                            )
                            .child(
                                IconButton::new(("delete", id), IconName::Trash2)
                                    .size(ControlSize::Sm)
                                    .tooltip("删除")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let result = this.history.remove(id);
                                        this.report(result, "已删除", cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .id(("text", id))
                    .w_full()
                    .min_w_0()
                    .h(px(45.0))
                    .truncate()
                    .text_size(px(14.0))
                    .line_height(px(25.0))
                    .child(text),
            )
            .into_any_element()
    }

    fn history_page(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb_4()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("{} 条记录", self.history.entries.len())),
                    )
                    .when(!self.history.entries.is_empty(), |el| {
                        el.child(
                            Button::new(
                                "clear-history",
                                if self.confirm_clear {
                                    "确认清空"
                                } else {
                                    "清空历史"
                                },
                            )
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.confirm_clear {
                                    let result = this.history.clear();
                                    this.confirm_clear = false;
                                    this.report(result, "已清空", cx);
                                } else {
                                    this.confirm_clear = true;
                                    cx.notify();
                                }
                            })),
                        )
                    }),
            )
            .when(self.history.entries.is_empty(), |el| {
                el.child(
                    div()
                        .h(px(300.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .text_color(rgb(MUTED))
                        .child(Icon::new(IconName::AudioLines).size(IconSize::Lg))
                        .child("暂无记录"),
                )
            })
            .when(!self.history.entries.is_empty(), |el| {
                let handle = self.history_scroll.0.borrow().base_handle.clone();
                el.child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .child(
                            uniform_list(
                                "voice-history-list",
                                self.history.entries.len(),
                                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                    range
                                        .filter_map(|ix| {
                                            this.history
                                                .entries
                                                .get(ix)
                                                .map(|entry| this.history_row(entry, cx))
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .track_scroll(&self.history_scroll)
                            .size_full(),
                        )
                        .child(
                            Scrollbar::new("voice-history-scrollbar", &handle, Axis::Vertical)
                                .auto(),
                        ),
                )
            })
            .into_any_element()
    }

    fn history_detail(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let entry = self
            .history
            .entries
            .iter()
            .find(|entry| Some(entry.id) == self.expanded)?;
        let weak = cx.entity().downgrade();
        let text = entry.text.clone();
        let copied = text.clone();
        let raw = entry.raw.clone();
        let polished = raw != text;
        Some(
            Dialog::new("voice-detail", "完整文本", move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.expanded = None;
                    cx.notify();
                });
            })
            .child(
                ScrollArea::new("voice-detail-scroll").h(px(360.0)).child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .when(polished, |el| {
                            el.child(div().text_sm().text_color(rgb(MUTED)).child("润色结果"))
                        })
                        .child(SelectableText::new("voice-detail-text", text))
                        .when(polished, |el| {
                            el.child(
                                div()
                                    .pt_3()
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child("识别原文"),
                            )
                            .child(SelectableText::new("voice-detail-raw", raw))
                        }),
                ),
            )
            .child(
                div().flex().justify_end().mt_3().child(
                    Button::new("copy-voice-detail", "复制全文")
                        .variant(ButtonVariant::Primary)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let result = platform::copy(&copied);
                            this.report(result, "已复制", cx);
                        })),
                ),
            )
            .into_any_element(),
        )
    }

    fn labeled(label: &'static str, input: impl IntoElement) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_sm().text_color(rgb(MUTED)).child(label))
            .child(input)
    }

    fn animation_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let scale = format!("{}", self.animation.scale);
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child("显示录音提示")
                    .child(
                        Switch::new("animation-enabled", self.animation.enabled)
                            .name("显示录音提示")
                            .on_change(move |on, _, cx| {
                                let _ = this.update(cx, |this, cx| {
                                    this.animation.enabled = on;
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .child(Self::labeled(
                "动画样式",
                Select::new(
                    "animation-style",
                    [
                        Choice::new("capsule", "内置声波 GIF"),
                        Choice::new("ribbon", "内置处理中 GIF"),
                        Choice::new("custom", "自定义 GIF"),
                    ],
                )
                .label("动画样式")
                .selected(self.animation.style.clone())
                .on_change(cx.listener(|this, value: &SharedString, _, cx| {
                    this.animation.style = value.to_string();
                    cx.notify();
                })),
            ))
            .when(self.animation.style == "custom", |el| {
                el.child(Self::labeled("GIF 文件", Input::new(&self.gif_path)))
                    .child(Button::new("choose-gif", "选择 GIF…").on_click(cx.listener(
                        |this, _, window, cx| {
                            if let Some(path) = desktop::choose_gif(window) {
                                this.gif_path
                                    .update(cx, |input, cx| input.set_text(path, cx));
                            }
                        },
                    )))
            })
            .child(Self::labeled(
                "位置",
                Select::new(
                    "animation-position",
                    [
                        Choice::new("bottom", "屏幕下方"),
                        Choice::new("top", "屏幕上方"),
                        Choice::new("right", "右下角"),
                    ],
                )
                .label("动画位置")
                .selected(self.animation.position.clone())
                .on_change(cx.listener(|this, value: &SharedString, _, cx| {
                    this.animation.position = value.to_string();
                    cx.notify();
                })),
            ))
            .child(Self::labeled(
                "大小",
                Select::new(
                    "animation-scale",
                    [
                        Choice::new("0.8", "小"),
                        Choice::new("1", "标准"),
                        Choice::new("1.3", "大"),
                    ],
                )
                .label("动画大小")
                .selected(scale)
                .on_change(cx.listener(|this, value: &SharedString, _, cx| {
                    this.animation.scale = value.parse().unwrap_or(1.0);
                    cx.notify();
                })),
            ))
            .child({
                let this = cx.entity().downgrade();
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child("识别与润色时保留提示")
                    .child(
                        Switch::new("processing", self.animation.show_processing)
                            .name("识别与润色时保留提示")
                            .on_change(move |on, _, cx| {
                                let _ = this.update(cx, |this, cx| {
                                    this.animation.show_processing = on;
                                    cx.notify();
                                });
                            }),
                    )
            })
            .child(
                div()
                    .pt_4()
                    .flex()
                    .gap_3()
                    .child(
                        Button::new("save-animation", "保存设置")
                            .variant(ButtonVariant::Primary)
                            .on_click(cx.listener(|this, _, _, cx| this.save_animation(cx))),
                    )
                    .child(
                        Button::new("preview", "预览动画")
                            .disabled(self.working)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.animation.gif_path =
                                    this.gif_path.read(cx).text().trim().into();
                                if this.animation.style == "custom"
                                    && let Err(error) =
                                        crate::overlay::validate_gif_path(&this.animation.gif_path)
                                {
                                    this.report(Err(error), "", cx);
                                    return;
                                }
                                let _ =
                                    this.events.try_send(Event::Preview(this.animation.clone()));
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for VoiceApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self.page {
            Page::History => "历史记录",
            Page::Api => "API 配置",
            Page::Animation => "动画设置",
            Page::ClipboardHistory => "历史记录",
            Page::ClipboardSettings => "设置",
        };
        let page = match self.page {
            Page::History => self.history_page(cx),
            Page::Api => self.api_page(window, cx),
            Page::Animation => self.animation_page(cx),
            Page::ClipboardHistory | Page::ClipboardSettings => self
                .clipboard
                .as_ref()
                .map(|clipboard| clipboard.clone().into_any_element())
                .unwrap_or_else(|| {
                    div()
                        .text_color(rgb(MUTED))
                        .child("剪贴板模块未启动")
                        .into_any_element()
                }),
        };
        let clipboard_page = matches!(self.page, Page::ClipboardHistory | Page::ClipboardSettings);
        FocusScope::new(&self.focus)
            .root()
            .relative()
            .size_full()
            .font_family("MiSans")
            .text_size(px(14.0))
            .text_color(rgb(INK))
            .bg(rgb(PAPER))
            .when_some(self.prompt_dialog(cx), |el, dialog| el.child(dialog))
            .when_some(self.history_detail(cx), |el, dialog| el.child(dialog))
            .child(
                div()
                    .size_full()
                    .flex()
                    .child(
                        div()
                            .w(px(182.0))
                            .flex_shrink_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .bg(rgb(SIDEBAR))
                            .border_r_1()
                            .border_color(rgb(LINE))
                            .p_3()
                            .child(
                                div()
                                    .h(px(58.0))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .px_2()
                                    .child(img("app-icon.png").w_9().h_9())
                                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Ducky")),
                            )
                            .child(
                                div()
                                    .mt_5()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .px_3()
                                            .pb_2()
                                            .flex()
                                            .justify_between()
                                            .text_xs()
                                            .text_color(rgb(MUTED))
                                            .child("语音输入")
                                            .child("右 Ctrl"),
                                    )
                                    .child(self.nav(
                                        Page::History,
                                        "历史记录",
                                        IconName::History,
                                        cx,
                                    ))
                                    .child(self.nav(
                                        Page::Api,
                                        "API 配置",
                                        IconName::SlidersHorizontal,
                                        cx,
                                    ))
                                    .child(self.nav(
                                        Page::Animation,
                                        "动画设置",
                                        IconName::AudioLines,
                                        cx,
                                    ))
                                    .child(
                                        div()
                                            .mt_4()
                                            .pt_4()
                                            .px_3()
                                            .border_t_1()
                                            .border_color(rgb(LINE))
                                            .flex()
                                            .justify_between()
                                            .text_xs()
                                            .text_color(rgb(MUTED))
                                            .child("剪贴板")
                                            .child("右 Alt"),
                                    )
                                    .child(self.nav(
                                        Page::ClipboardHistory,
                                        "历史记录",
                                        IconName::Clipboard,
                                        cx,
                                    ))
                                    .child(self.nav(
                                        Page::ClipboardSettings,
                                        "设置",
                                        IconName::Settings,
                                        cx,
                                    )),
                            )
                            .child(div().flex_1())
                            .child({
                                let this = cx.entity().downgrade();
                                div()
                                    .px_2()
                                    .py_3()
                                    .border_t_1()
                                    .border_color(rgb(LINE))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(MUTED))
                                            .child("开机静默启动"),
                                    )
                                    .child(
                                        Switch::new("silent-startup", self.startup_enabled)
                                            .name("开机静默启动")
                                            .on_change(move |enabled, _, cx| {
                                                let _ = this.update(cx, |this, cx| {
                                                    this.set_startup(enabled, cx)
                                                });
                                            }),
                                    )
                            })
                            .child(
                                Button::new("hide-to-tray", "收起到托盘")
                                    .variant(ButtonVariant::Ghost)
                                    .size(ControlSize::Sm)
                                    .on_click(|_, _, _| desktop::hide_main()),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .px_7()
                                    .pt_5()
                                    .pb_3()
                                    .flex_shrink_0()
                                    .flex()
                                    .flex_col()
                                    .when(clipboard_page, |el| {
                                        el.child(
                                            div().text_xs().text_color(rgb(MUTED)).child("剪贴板"),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_size(if clipboard_page {
                                                px(20.0)
                                            } else {
                                                px(16.0)
                                            })
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(title),
                                    ),
                            )
                            .child(
                                div()
                                    .id("page-scroll")
                                    .flex_1()
                                    .min_h_0()
                                    .when(!clipboard_page && self.page != Page::History, |el| {
                                        el.overflow_y_scroll()
                                    })
                                    .px_7()
                                    .pt_3()
                                    .pb_7()
                                    .child(page),
                            )
                            .when(self.page == Page::Api, |el| el.child(self.api_footer(cx)))
                            .when(!self.status.is_empty() && !self.working, |el| {
                                el.child(
                                    div()
                                        .px_7()
                                        .py_2()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .text_xs()
                                        .text_color(rgb(if self.error { 0xa55942 } else { MUTED }))
                                        .child(div().flex_1().child(self.status.clone()))
                                        .when_some(self.unsaved_result.clone(), |el, text| {
                                            el.child(
                                                Button::new("copy-unsaved", "复制本次结果")
                                                    .size(ControlSize::Sm)
                                                    .on_click(move |_, _, _| {
                                                        let _ = platform::copy(&text);
                                                    }),
                                            )
                                        }),
                                )
                            }),
                    ),
            )
    }
}
