use super::*;
use crate::{
    api,
    config::{Language, Provider},
};
use ely_core::{
    forms::{float, surface},
    overlays::Dialog,
};
use std::time::{Duration, Instant};

const SURFACE: u32 = PAPER;
const ACCENT: u32 = 0x75866b;
const ERROR: u32 = 0xa55942;

type OnSegment = std::rc::Rc<dyn Fn(&SharedString, &mut Window, &mut App)>;
/// The Ely controls use the app's ordinary field radius. Keep these four
/// selectors genuinely pill-shaped without changing any global color/radius.
#[derive(IntoElement)]
struct Capsule {
    id: ElementId,
    selected: SharedString,
    choices: Vec<(SharedString, SharedString)>,
    on_change: Option<OnSegment>,
}
impl Capsule {
    fn new(id: impl Into<ElementId>, selected: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            selected: selected.into(),
            choices: Vec::new(),
            on_change: None,
        }
    }
    fn segment(
        mut self,
        value: impl Into<SharedString>,
        label: impl Into<SharedString>,
        _: Option<IconName>,
    ) -> Self {
        self.choices.push((value.into(), label.into()));
        self
    }
    fn on_change(
        mut self,
        callback: impl Fn(&SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_change = Some(std::rc::Rc::new(callback));
        self
    }
}
impl RenderOnce for Capsule {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let choices = std::rc::Rc::new(self.choices);
        div()
            .flex()
            .w_full()
            .p(px(3.))
            .rounded_full()
            .bg(rgb(0xeee9d8))
            .children(choices.iter().enumerate().map(|(index, (value, label))| {
                let focus = ely_core::primitives::tab_stop(
                    (self.id.clone(), format!("segment-{index}")).into(),
                    true,
                    window,
                    cx,
                );
                let focused = focus.is_focused(window);
                let click = self.on_change.clone();
                let keys = click.clone();
                let value = value.clone();
                let keyboard_value = value.clone();
                let options = choices.clone();
                div()
                    .id((self.id.clone(), format!("segment-{index}")))
                    .role(Role::RadioButton)
                    .aria_label(label.clone())
                    .aria_toggled((value == self.selected).into())
                    .track_focus(&focus)
                    .flex_1()
                    .min_w_0()
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(if focused {
                        0x89997b
                    } else if value == self.selected {
                        LINE
                    } else {
                        0xeee9d8
                    }))
                    .bg(rgb(if value == self.selected {
                        SURFACE
                    } else {
                        0xeee9d8
                    }))
                    .cursor_pointer()
                    .text_sm()
                    .font_weight(if value == self.selected {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .child(label.clone())
                    .on_click(move |_, window, cx| {
                        window.focus(&focus, cx);
                        if let Some(callback) = &click {
                            callback(&value, window, cx);
                        }
                    })
                    .on_key_down(move |event, window, cx| {
                        let chosen = match event.keystroke.key.as_str() {
                            "space" | "enter" => keyboard_value.clone(),
                            "left" => options[(index + options.len() - 1) % options.len()]
                                .0
                                .clone(),
                            "right" => options[(index + 1) % options.len()].0.clone(),
                            _ => return,
                        };
                        cx.stop_propagation();
                        if let Some(callback) = &keys {
                            callback(&chosen, window, cx);
                        }
                    })
            }))
    }
}

#[derive(Default)]
struct TestState {
    text: String,
    error: bool,
    cancelled: bool,
    running: bool,
    started: Option<Instant>,
    elapsed: f64,
    fingerprint: String,
}
struct Job {
    id: u64,
    slot: usize,
    cancel: async_channel::Sender<()>,
}
enum Update {
    Progress(String),
    Done(Result<String, String>),
    Models(Result<Vec<api::Model>, String>),
}

pub(super) struct ApiState {
    provider: Provider,
    mimo_stream: bool,
    asr_stream: bool,
    language: Language,
    router_stream: bool,
    router_key: Entity<TextInput>,
    router_model: Entity<TextInput>,
    router_prompt: Entity<TextInput>,
    search: Entity<TextInput>,
    editor: Entity<TextInput>,
    editing: Option<bool>, // true = polish; false = recognition
    models: Vec<api::Model>,
    model_open: bool,
    model_manual: bool,
    model_bounds: Bounds<Pixels>,
    highlighted: usize,
    tests: [TestState; 5], // MiMo multimodal, OpenRouter, MiMo ASR, polish, models
    active: Option<Job>,
    sequence: u64,
    saved: bool,
}

impl ApiState {
    pub(super) fn new(config: &Config, window: &mut Window, cx: &mut App) -> Self {
        Self {
            provider: config.provider,
            mimo_stream: config.mimo_stream,
            asr_stream: config.asr_stream,
            language: config.asr_language,
            router_stream: config.openrouter_stream,
            router_key: field(
                window,
                cx,
                "OpenRouter API Key",
                &config.openrouter_key,
                true,
                false,
            ),
            router_model: field(
                window,
                cx,
                "模型 ID",
                &config.openrouter_model,
                false,
                false,
            ),
            router_prompt: field(
                window,
                cx,
                "OpenRouter 识别提示词",
                &config.openrouter_prompt,
                false,
                true,
            ),
            search: field(window, cx, "搜索模型名称或 ID", "", false, false),
            editor: cx.new(|cx| {
                TextInput::new(window, cx)
                    .label("提示词编辑")
                    .multi_line(12, 20)
            }),
            editing: None,
            models: load_models(config),
            model_open: false,
            model_manual: false,
            model_bounds: Bounds::default(),
            highlighted: 0,
            tests: std::array::from_fn(|_| TestState::default()),
            active: None,
            sequence: 0,
            saved: false,
        }
    }
}

fn load_models(config: &Config) -> Vec<api::Model> {
    let Ok(path) = config.model_cache_path() else {
        return Vec::new();
    };
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    use std::io::Read;
    let mut bytes = Vec::new();
    if file.take(2 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    serde_json::from_slice::<Vec<api::Model>>(&bytes).unwrap_or_default()
}

impl VoiceApp {
    /// Isolated UI smoke exercises draft/save/revert and renders every provider.
    /// No credentials are loaded and no external API is called.
    pub fn smoke_api(&mut self, phase: usize, cx: &mut Context<Self>) -> Vec<(&'static str, bool)> {
        self.page = Page::Api;
        let mut checks = Vec::new();
        match phase {
            0 => {
                self.recognition_mode = RecognitionMode::Asr;
                self.mimo_key
                    .update(cx, |input, cx| input.set_text("isolated-fixture-key", cx));
                self.api.language = Language::Zh;
                checks.push(("api_draft_dirty", self.dirty(cx)));
                self.save_api(cx);
                checks.push((
                    "api_draft_saved",
                    !self.dirty(cx) && self.draft_api(cx).validate_recognition().is_ok(),
                ));
                self.api.language = Language::En;
                self.revert_api(cx);
                checks.push((
                    "api_revert",
                    self.api.language == Language::Zh && !self.dirty(cx),
                ));
                self.polish_enabled = true;
            }
            1 => {
                self.recognition_mode = RecognitionMode::Multimodal;
                self.api.provider = Provider::Mimo;
                self.polish_enabled = false;
                checks.push((
                    "mimo_modes_share_key",
                    self.draft_api(cx).mimo_key == "isolated-fixture-key",
                ));
                let fingerprint = Self::test_fingerprint(&self.draft_api(cx), 0);
                self.api.tests[0] = TestState {
                    fingerprint,
                    text: "界面自检示例结果".into(),
                    elapsed: 1.2,
                    ..TestState::default()
                };
            }
            2 => {
                self.api.provider = Provider::Openrouter;
                self.api.models = vec![api::Model {
                    id: "example/audio-model".into(),
                    name: "示例音频模型（离线自检）".into(),
                }];
                self.api.model_open = true;
                self.api
                    .search
                    .update(cx, |input, cx| input.set_text("示例", cx));
                checks.push(("model_search", self.filtered_models(cx).len() == 1));
                checks.push((
                    "router_key_separate",
                    self.api.router_key.read(cx).text().is_empty(),
                ));
            }
            3 => {
                self.api.model_open = false;
                self.api.editor.update(cx, |input, cx| {
                    input.set_text(DEFAULT_TRANSCRIPTION_PROMPT, cx)
                });
                self.api.editing = Some(false);
                checks.push(("prompt_editor", self.prompt_dialog(cx).is_some()));
            }
            _ => {
                self.api.editing = None;
                self.revert_api(cx);
            }
        }
        cx.notify();
        checks
    }

    fn recognition_slot(&self) -> usize {
        if self.recognition_mode == RecognitionMode::Asr {
            2
        } else if self.api.provider == Provider::Openrouter {
            1
        } else {
            0
        }
    }
    pub(super) fn api_inputs(&self) -> Vec<Entity<TextInput>> {
        vec![
            self.mimo_key.clone(),
            self.mimo_model.clone(),
            self.transcription_prompt.clone(),
            self.api.router_key.clone(),
            self.api.router_model.clone(),
            self.api.router_prompt.clone(),
            self.api.search.clone(),
            self.polish_url.clone(),
            self.polish_key.clone(),
            self.polish_model.clone(),
            self.polish_prompt.clone(),
        ]
    }
    fn draft_api(&self, cx: &App) -> Config {
        let mut c = self
            .shared
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        c.recognition_mode = self.recognition_mode;
        c.provider = self.api.provider;
        c.mimo_key = self.mimo_key.read(cx).text().trim().into();
        c.mimo_model = self.mimo_model.read(cx).text().trim().into();
        c.transcription_prompt = self.transcription_prompt.read(cx).text().into();
        c.mimo_stream = self.api.mimo_stream;
        c.asr_stream = self.api.asr_stream;
        c.asr_language = self.api.language;
        c.openrouter_key = self.api.router_key.read(cx).text().trim().into();
        c.openrouter_model = self.api.router_model.read(cx).text().trim().into();
        c.openrouter_prompt = self.api.router_prompt.read(cx).text().into();
        c.openrouter_stream = self.api.router_stream;
        c.polish_enabled = self.polish_enabled;
        c.polish_url = self.polish_url.read(cx).text().trim().into();
        c.polish_key = self.polish_key.read(cx).text().trim().into();
        c.polish_model = self.polish_model.read(cx).text().trim().into();
        c.polish_prompt = self.polish_prompt.read(cx).text().into();
        c
    }
    fn api_fingerprint(c: &Config) -> String {
        // Include skipped plaintext keys in memory only; never log or persist this.
        serde_json::json!([
            c.recognition_mode,
            c.provider,
            c.mimo_key,
            c.mimo_model,
            c.transcription_prompt,
            c.mimo_stream,
            c.asr_language,
            c.asr_stream,
            c.openrouter_key,
            c.openrouter_model,
            c.openrouter_prompt,
            c.openrouter_stream,
            c.polish_enabled,
            c.polish_url,
            c.polish_key,
            c.polish_model,
            c.polish_prompt
        ])
        .to_string()
    }
    fn test_fingerprint(c: &Config, slot: usize) -> String {
        match slot {
            0 => serde_json::json!([
                c.mimo_key,
                c.mimo_model,
                c.transcription_prompt,
                c.mimo_stream
            ]),
            1 => serde_json::json!([
                c.openrouter_key,
                c.openrouter_model,
                c.openrouter_prompt,
                c.openrouter_stream
            ]),
            2 => serde_json::json!([c.mimo_key, c.asr_language, c.asr_stream]),
            3 => serde_json::json!([c.polish_url, c.polish_key, c.polish_model, c.polish_prompt]),
            _ => serde_json::json!(c.openrouter_key),
        }
        .to_string()
    }
    fn dirty(&self, cx: &App) -> bool {
        Self::api_fingerprint(&self.draft_api(cx))
            != Self::api_fingerprint(&self.shared.read().unwrap_or_else(|e| e.into_inner()))
    }
    fn save_api(&mut self, cx: &mut Context<Self>) {
        let config = self.draft_api(cx);
        match config.save() {
            Ok(()) => {
                *self.shared.write().unwrap_or_else(|e| e.into_inner()) = config;
                self.api.saved = true;
                self.error = false;
                self.status.clear();
            }
            Err(e) => {
                self.error = true;
                self.status = format!("保存失败：{e:#}");
            }
        }
        cx.notify();
    }
    fn revert_api(&mut self, cx: &mut Context<Self>) {
        self.cancel_test(cx);
        let c = self
            .shared
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.recognition_mode = c.recognition_mode;
        self.api.provider = c.provider;
        self.api.mimo_stream = c.mimo_stream;
        self.api.asr_stream = c.asr_stream;
        self.api.language = c.asr_language;
        self.api.router_stream = c.openrouter_stream;
        for (input, value) in [
            (self.mimo_key.clone(), c.mimo_key),
            (self.mimo_model.clone(), c.mimo_model),
            (self.transcription_prompt.clone(), c.transcription_prompt),
            (self.api.router_key.clone(), c.openrouter_key),
            (self.api.router_model.clone(), c.openrouter_model),
            (self.api.router_prompt.clone(), c.openrouter_prompt),
            (self.polish_url.clone(), c.polish_url),
            (self.polish_key.clone(), c.polish_key),
            (self.polish_model.clone(), c.polish_model),
            (self.polish_prompt.clone(), c.polish_prompt),
        ] {
            input.update(cx, |input, cx| input.set_text(value, cx));
        }
        self.polish_enabled = c.polish_enabled;
        self.api.model_open = false;
        self.api.saved = false;
        cx.notify();
    }
    fn cancel_test(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = self.api.active.take() {
            let _ = job.cancel.try_send(());
            let state = &mut self.api.tests[job.slot];
            state.running = false;
            state.cancelled = true;
            state.text.clear();
            state.elapsed = state
                .started
                .map(|s| s.elapsed().as_secs_f64())
                .unwrap_or_default();
        }
        cx.notify();
    }
    fn start_test(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.api.active.is_some() {
            return;
        }
        let config = self.draft_api(cx);
        let valid = if slot == 3 {
            config.validate_polish()
        } else if slot == 4 {
            if config.openrouter_key.trim().is_empty() {
                Err(anyhow::anyhow!("请填写 OpenRouter API Key"))
            } else {
                Ok(())
            }
        } else {
            config.validate_recognition()
        };
        let state = &mut self.api.tests[slot];
        *state = TestState {
            fingerprint: Self::test_fingerprint(&config, slot),
            ..TestState::default()
        };
        if let Err(e) = valid {
            state.error = true;
            state.text = e.to_string();
            cx.notify();
            return;
        }
        state.running = true;
        state.started = Some(Instant::now());
        self.api.sequence += 1;
        let id = self.api.sequence;
        let (cancel, receive_cancel) = async_channel::bounded(1);
        self.api.active = Some(Job { id, slot, cancel });
        // A fast token stream must not queue unbounded copies of partial text.
        let (send, receive) = async_channel::bounded(8);
        let spawn = std::thread::Builder::new()
            .name("api-settings-test".into())
            .spawn(move || {
                let result = api::client();
                let update = if slot == 4 {
                    Update::Models(
                        result
                            .and_then(|c| api::models(&c, &config.openrouter_key, &receive_cancel))
                            .map_err(|e| e.to_string()),
                    )
                } else {
                    Update::Done(
                        result
                            .and_then(|c| {
                                if slot == 3 {
                                    api::polish_with_cancel(
                                        &c,
                                        &config,
                                        "那个，我们明天下午三点开会，呃，记得带笔记本。",
                                        Some(&receive_cancel),
                                    )
                                } else {
                                    api::transcribe_wav(
                                        &c,
                                        &config,
                                        include_bytes!("../../assets/test-speech.wav"),
                                        Some(&receive_cancel),
                                        &mut |text| {
                                            let _ = send.try_send(Update::Progress(text));
                                        },
                                    )
                                }
                            })
                            .map_err(|e| e.to_string()),
                    )
                };
                let _ = send.send_blocking(update);
            });
        if let Err(e) = spawn {
            self.api.active = None;
            let state = &mut self.api.tests[slot];
            state.running = false;
            state.error = true;
            state.text = format!("启动测试失败：{e}");
            cx.notify();
            return;
        }
        cx.spawn(async move |this, cx| {
            while let Ok(update) = receive.recv().await {
                let _ = this.update(cx, |this, cx| {
                    if !this.api.active.as_ref().is_some_and(|j| j.id == id) {
                        return;
                    }
                    let state = &mut this.api.tests[slot];
                    match update {
                        Update::Progress(text) => state.text = text,
                        Update::Done(result) => {
                            state.running = false;
                            state.elapsed = state.started.unwrap().elapsed().as_secs_f64();
                            state.error = result.is_err();
                            state.text = result.unwrap_or_else(|e| e);
                            this.api.active = None;
                        }
                        Update::Models(result) => {
                            state.running = false;
                            state.error = result.is_err();
                            match result {
                                Ok(models) => {
                                    state.text =
                                        format!("已获取 {} 个支持音频输入的模型", models.len());
                                    if let Ok(path) = this
                                        .shared
                                        .read()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .model_cache_path()
                                    {
                                        let _ = std::fs::create_dir_all(path.parent().unwrap());
                                        if let Ok(bytes) = serde_json::to_vec(&models) {
                                            let _ = std::fs::write(path, bytes);
                                        }
                                    }
                                    this.api.models = models;
                                }
                                Err(e) => state.text = e,
                            }
                            this.api.active = None;
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let active = this
                    .update(cx, |this, cx| {
                        let active = this.api.active.as_ref().is_some_and(|j| j.id == id);
                        if active {
                            cx.notify();
                        }
                        active
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }
    fn api_row(label: &'static str, content: impl IntoElement) -> Div {
        div()
            .flex()
            .items_start()
            .gap_4()
            .child(
                div()
                    .w(px(90.))
                    .flex_shrink_0()
                    .pt_2()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(label),
            )
            .child(div().flex_1().min_w_0().child(content))
    }
    fn api_card() -> Div {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .py_5()
            .border_b_1()
            .border_color(rgb(LINE))
            .bg(rgb(SURFACE))
    }
    fn caption(text: impl Into<SharedString>) -> Div {
        div().text_xs().text_color(rgb(MUTED)).child(text.into())
    }
    fn secret(&self, input: &Entity<TextInput>, shared: Option<&'static str>, cx: &App) -> Input {
        let state = input.clone();
        let masked = input.read(cx).is_masked();
        Input::new(input).suffix(
            div()
                .flex()
                .items_center()
                .gap_2()
                .when_some(shared, |el, text| {
                    el.child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(SELECTED))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(text),
                    )
                })
                .child(
                    Button::new(
                        ("show-key", input.entity_id()),
                        if masked { "显示" } else { "隐藏" },
                    )
                    .variant(ButtonVariant::Ghost)
                    .size(ControlSize::Sm)
                    .on_click(move |_, _, cx| {
                        state.update(cx, |input, cx| input.set_masked(!input.is_masked(), cx));
                    }),
                ),
        )
    }
    fn active_prompt(&self) -> Entity<TextInput> {
        if self.api.provider == Provider::Openrouter {
            self.api.router_prompt.clone()
        } else {
            self.transcription_prompt.clone()
        }
    }
    fn prompt_field(&self, polish: bool, cx: &mut Context<Self>) -> Div {
        let input = if polish {
            self.polish_prompt.clone()
        } else {
            self.active_prompt()
        };
        let reset = input.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(Input::new(&input))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        Button::new(
                            if polish {
                                "expand-polish"
                            } else {
                                "expand-recognition"
                            },
                            "展开编辑",
                        )
                        .variant(ButtonVariant::Ghost)
                        .size(ControlSize::Sm)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let input = if polish {
                                this.polish_prompt.clone()
                            } else {
                                this.active_prompt()
                            };
                            let text = input.read(cx).text().to_string();
                            this.api
                                .editor
                                .update(cx, |input, cx| input.set_text(text, cx));
                            this.api.editing = Some(polish);
                            cx.notify();
                        })),
                    )
                    .child(
                        Button::new(
                            if polish {
                                "reset-polish"
                            } else {
                                "reset-recognition"
                            },
                            "恢复默认",
                        )
                        .variant(ButtonVariant::Ghost)
                        .size(ControlSize::Sm)
                        .on_click(move |_, _, cx| {
                            reset.update(cx, |input, cx| {
                                input.set_text(
                                    if polish {
                                        DEFAULT_POLISH_PROMPT
                                    } else {
                                        DEFAULT_TRANSCRIPTION_PROMPT
                                    },
                                    cx,
                                )
                            })
                        }),
                    ),
            )
    }
    pub(super) fn prompt_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let polish = self.api.editing?;
        let weak = cx.entity().downgrade();
        let apply = weak.clone();
        Some(
            Dialog::new(
                "prompt-dialog",
                if polish {
                    "润色提示词"
                } else {
                    "识别提示词"
                },
                move |_, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.api.editing = None;
                        cx.notify();
                    });
                },
            )
            .child(Input::new(&self.api.editor))
            .action(|close| {
                Button::new("prompt-cancel", "取消")
                    .on_click(move |_, window, cx| close(window, cx))
            })
            .action(move |close| {
                Button::new("prompt-apply", "应用")
                    .variant(ButtonVariant::Primary)
                    .on_click(move |_, window, cx| {
                        let _ = apply.update(cx, |this, cx| {
                            let text = this.api.editor.read(cx).text().to_string();
                            let input = if polish {
                                this.polish_prompt.clone()
                            } else {
                                this.active_prompt()
                            };
                            input.update(cx, |input, cx| input.set_text(text, cx));
                        });
                        close(window, cx);
                    })
            })
            .into_any_element(),
        )
    }
    fn test_panel(&self, slot: usize, cx: &mut Context<Self>) -> Div {
        let state = &self.api.tests[slot];
        let expired = !state.fingerprint.is_empty()
            && state.fingerprint != Self::test_fingerprint(&self.draft_api(cx), slot);
        let active = self.api.active.as_ref().is_some_and(|j| j.slot == slot);
        let seconds = if state.running {
            state.started.unwrap().elapsed().as_secs_f64()
        } else {
            state.elapsed
        };
        let copied = state.text.clone();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new(
                            ("test", slot),
                            if active {
                                "取消测试"
                            } else if slot == 3 {
                                "测试润色"
                            } else {
                                "测试识别"
                            },
                        )
                        .disabled(self.api.active.is_some() && !active)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if active {
                                this.cancel_test(cx);
                            } else {
                                this.start_test(slot, cx);
                            }
                        })),
                    )
                    .child(Self::caption(if state.running {
                        format!("处理中 · 已用 {seconds:.1} 秒")
                    } else if state.cancelled {
                        "已取消".into()
                    } else if slot == 3 {
                        "使用示例文稿，不录音、不粘贴".into()
                    } else {
                        "发送内置测试音频，可能产生费用".into()
                    })),
            )
            .when(!state.text.is_empty(), |el| {
                el.child(
                    div()
                        .pl_3()
                        .py_2()
                        .border_l_2()
                        .border_color(rgb(LINE))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(if state.error { ERROR } else { INK }))
                                        .child(if expired {
                                            "配置已更改，请重新测试".into()
                                        } else if state.running {
                                            "正在返回文字…".into()
                                        } else if state.error {
                                            "测试失败".into()
                                        } else {
                                            format!("✓ 测试通过 · {seconds:.1} 秒")
                                        }),
                                )
                                .when(!state.error && !state.running, |el| {
                                    el.child(
                                        Button::new(("copy-test", slot), "复制结果")
                                            .variant(ButtonVariant::Ghost)
                                            .size(ControlSize::Sm)
                                            .on_click(move |_, _, _| {
                                                let _ = platform::copy(&copied);
                                            }),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .text_sm()
                                .line_height(px(23.))
                                .child(state.text.clone()),
                        ),
                )
            })
    }
    fn response_choice(&self, cx: &mut Context<Self>) -> Capsule {
        let slot = self.recognition_slot();
        let stream = match slot {
            0 => self.api.mimo_stream,
            1 => self.api.router_stream,
            _ => self.api.asr_stream,
        };
        Capsule::new("response-mode", if stream { "stream" } else { "normal" })
            .segment("normal", "非流式", None)
            .segment("stream", "流式", None)
            .on_change(cx.listener(move |this, value: &SharedString, _, cx| {
                match slot {
                    0 => this.api.mimo_stream = value == "stream",
                    1 => this.api.router_stream = value == "stream",
                    _ => this.api.asr_stream = value == "stream",
                }
                cx.notify();
            }))
    }
    fn filtered_models(&self, cx: &App) -> Vec<api::Model> {
        let query = self.api.search.read(cx).text().trim().to_lowercase();
        self.api
            .models
            .iter()
            .filter(|m| {
                m.id.to_lowercase().contains(&query) || m.name.to_lowercase().contains(&query)
            })
            .cloned()
            .collect()
    }
    fn router_picker(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let selected = self.api.router_model.read(cx).text().to_string();
        let title = self
            .api
            .models
            .iter()
            .find(|m| m.id == selected)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| {
                if selected.is_empty() {
                    "请选择模型".into()
                } else {
                    selected.clone()
                }
            });
        let weak = cx.entity().downgrade();
        div()
            .relative()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .relative()
                            .child(Button::new("model-menu", title).on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.api.model_open = !this.api.model_open;
                                    this.api.highlighted = 0;
                                    if this.api.model_open {
                                        this.api
                                            .search
                                            .update(cx, |input, cx| input.set_text("", cx));
                                        let focus = this.api.search.read(cx).focus().clone();
                                        window.focus(&focus, cx);
                                    }
                                    cx.notify();
                                },
                            )))
                            .child(
                                canvas(
                                    move |bounds, _, cx| {
                                        let _ = weak
                                            .update(cx, |this, _| this.api.model_bounds = bounds);
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .size_full(),
                            ),
                    )
                    .child(
                        Button::new(
                            "refresh-models",
                            if self.api.tests[4].running {
                                "取消"
                            } else {
                                "刷新"
                            },
                        )
                        .disabled(self.api.active.is_some() && !self.api.tests[4].running)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.api.tests[4].running {
                                this.cancel_test(cx)
                            } else {
                                this.start_test(4, cx)
                            }
                        })),
                    ),
            )
            .when(self.api.model_manual, |el| {
                el.child(Input::new(&self.api.router_model))
            })
            .when(!selected.is_empty(), |el| {
                el.child(Self::caption(format!(
                    "{}{}",
                    selected,
                    if self.api.models.iter().any(|m| m.id == selected) {
                        ""
                    } else {
                        " · 能力未验证"
                    }
                )))
            })
            .when(
                !self.api.tests[4].text.is_empty() && !self.api.model_open,
                |el| el.child(Self::caption(self.api.tests[4].text.clone())),
            )
            .when(self.api.model_open, |el| {
                let models = self.filtered_models(cx);
                let menu = surface("model-options", cx)
                    .w(self.api.model_bounds.size.width.max(px(260.)))
                    .max_w(window.viewport_size().width - px(60.))
                    .p_2()
                    .occlude()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.api.model_open = false;
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        let models = this.filtered_models(cx);
                        match event.keystroke.key.as_str() {
                            "escape" => this.api.model_open = false,
                            "down" => {
                                this.api.highlighted =
                                    (this.api.highlighted + 1).min(models.len().saturating_sub(1))
                            }
                            "up" => this.api.highlighted = this.api.highlighted.saturating_sub(1),
                            "enter" => {
                                if let Some(model) = models
                                    .get(this.api.highlighted.min(models.len().saturating_sub(1)))
                                {
                                    this.api.router_model.update(cx, |input, cx| {
                                        input.set_text(model.id.clone(), cx)
                                    });
                                    this.api.model_manual = false;
                                    this.api.model_open = false;
                                }
                            }
                            _ => return,
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .child(Input::new(&self.api.search))
                    .child(
                        div()
                            .id("models-scroll")
                            .max_h(px(210.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .when(models.is_empty(), |el| {
                                el.child(Self::caption(if self.api.tests[4].running {
                                    "正在获取模型…"
                                } else if self.api.models.is_empty() {
                                    "暂无模型，请刷新或手动填写"
                                } else {
                                    "没有匹配的模型"
                                }))
                            })
                            .children(models.iter().enumerate().map(|(index, model)| {
                                let id = model.id.clone();
                                div()
                                    .id(("audio-model", index))
                                    .role(Role::Button)
                                    .aria_label(model.name.clone())
                                    .p_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(rgb(
                                        if index == self.api.highlighted || model.id == selected {
                                            SELECTED
                                        } else {
                                            SURFACE
                                        },
                                    ))
                                    .hover(|s| s.bg(rgb(SELECTED)))
                                    .child(div().text_sm().child(model.name.clone()))
                                    .child(Self::caption(model.id.clone()))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.api
                                            .router_model
                                            .update(cx, |input, cx| input.set_text(id.clone(), cx));
                                        this.api.model_open = false;
                                        this.api.model_manual = false;
                                        cx.notify();
                                    }))
                            })),
                    )
                    .child(
                        div()
                            .border_t_1()
                            .border_color(rgb(LINE))
                            .pt_1()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(Self::caption(format!(
                                "{} 个音频模型",
                                self.api.models.len()
                            )))
                            .child(
                                Button::new("manual-model", "手动填写模型 ID")
                                    .variant(ButtonVariant::Ghost)
                                    .size(ControlSize::Sm)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.api.model_manual = true;
                                        this.api.model_open = false;
                                        let focus = this.api.router_model.read(cx).focus().clone();
                                        window.focus(&focus, cx);
                                        cx.notify();
                                    })),
                            ),
                    );
                el.child(float(
                    "model-float",
                    self.api.model_bounds,
                    6,
                    menu,
                    window,
                    cx,
                ))
            })
    }
    pub(super) fn api_page(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let slot = self.recognition_slot();
        let router = slot == 1;
        let recognition = Self::api_card()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("语音识别"))
                    .child(
                        Button::new("get-key", "获取 Key ↗")
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .on_click(move |_, _, cx| {
                                cx.open_url(if router {
                                    "https://openrouter.ai/keys"
                                } else {
                                    "https://platform.xiaomimimo.com/"
                                })
                            }),
                    ),
            )
            .child(
                div().w(px(220.)).child(
                    Capsule::new(
                        "recognition-mode",
                        if slot == 2 { "asr" } else { "multimodal" },
                    )
                    .segment("multimodal", "多模态", None)
                    .segment("asr", "普通 ASR", None)
                    .on_change(cx.listener(
                        |this, value: &SharedString, _, cx| {
                            this.cancel_test(cx);
                            this.api.model_open = false;
                            this.recognition_mode = if value == "asr" {
                                RecognitionMode::Asr
                            } else {
                                RecognitionMode::Multimodal
                            };
                            cx.notify();
                        },
                    )),
                ),
            )
            .child(Self::api_row(
                "服务商",
                if slot == 2 {
                    div()
                        .py_2()
                        .child("MiMo")
                        .child(Self::caption("普通 ASR 固定使用"))
                        .into_any_element()
                } else {
                    div()
                        .w(px(240.))
                        .child(
                            Capsule::new(
                                "recognition-provider",
                                if router { "openrouter" } else { "mimo" },
                            )
                            .segment("mimo", "MiMo", None)
                            .segment("openrouter", "OpenRouter", None)
                            .on_change(cx.listener(
                                |this, value: &SharedString, _, cx| {
                                    this.cancel_test(cx);
                                    this.api.model_open = false;
                                    this.api.provider = if value == "openrouter" {
                                        Provider::Openrouter
                                    } else {
                                        Provider::Mimo
                                    };
                                    cx.notify();
                                },
                            )),
                        )
                        .into_any_element()
                },
            ))
            .child(Self::api_row(
                "API Key",
                self.secret(
                    if router {
                        &self.api.router_key
                    } else {
                        &self.mimo_key
                    },
                    Some(if router {
                        "仅用于 OpenRouter"
                    } else {
                        "MiMo 两种模式共用"
                    }),
                    cx,
                ),
            ))
            .child(Self::api_row(
                "模型",
                if slot == 2 {
                    div()
                        .p_2()
                        .rounded_md()
                        .bg(rgb(PAPER))
                        .border_1()
                        .border_color(rgb(LINE))
                        .flex()
                        .justify_between()
                        .child("mimo-v2.5-asr")
                        .child(Self::caption("固定"))
                        .into_any_element()
                } else if router {
                    self.router_picker(window, cx).into_any_element()
                } else {
                    Select::new(
                        "mimo-model",
                        [
                            "mimo-v2.6-flash",
                            "mimo-v2.6-pro",
                            "mimo-v2.6-pro-ultraspeed",
                        ]
                        .into_iter()
                        .chain(
                            (!matches!(
                                self.mimo_model.read(cx).text(),
                                "mimo-v2.6-flash" | "mimo-v2.6-pro" | "mimo-v2.6-pro-ultraspeed"
                            ))
                            .then(|| self.mimo_model.read(cx).text()),
                        )
                        .map(|id| Choice::new(id.to_string(), id.to_string())),
                    )
                    .label("MiMo 多模态模型")
                    .selected(self.mimo_model.read(cx).text().to_string())
                    .on_change(cx.listener(|this, value: &SharedString, _, cx| {
                        this.mimo_model
                            .update(cx, |input, cx| input.set_text(value.clone(), cx))
                    }))
                    .into_any_element()
                },
            ))
            .when(slot == 2, |el| {
                el.child(Self::api_row(
                    "识别语言",
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            div().w(px(210.)).child(
                                Capsule::new("asr-language", self.api.language.code())
                                    .segment("auto", "自动", None)
                                    .segment("zh", "中文", None)
                                    .segment("en", "英文", None)
                                    .on_change(cx.listener(|this, value: &SharedString, _, cx| {
                                        this.api.language = match value.as_ref() {
                                            "zh" => Language::Zh,
                                            "en" => Language::En,
                                            _ => Language::Auto,
                                        };
                                        cx.notify();
                                    })),
                            ),
                        )
                        .when(self.api.language == Language::Auto, |el| {
                            el.child(Self::caption("自动适合中英文混合输入"))
                        }),
                ))
            })
            .when(slot != 2, |el| {
                el.child(Self::api_row("识别提示词", self.prompt_field(false, cx)))
            })
            .child(Self::api_row(
                "响应方式",
                div().w(px(180.)).child(self.response_choice(cx)),
            ))
            .child(Self::api_row("测试", self.test_panel(slot, cx)));
        let weak = cx.entity().downgrade();
        let polish = Self::api_card()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("文本润色"))
                            .child(Self::caption("识别完成后整理文稿，独立于上方识别方式")),
                    )
                    .child(
                        Switch::new("polish-enabled", self.polish_enabled)
                            .name("启用文本润色")
                            .on_change(move |on, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    if !on && this.api.active.as_ref().is_some_and(|j| j.slot == 3)
                                    {
                                        this.cancel_test(cx);
                                    }
                                    this.polish_enabled = on;
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .when(self.polish_enabled, |el| {
                el.child(Self::api_row("API 地址", Input::new(&self.polish_url)))
                    .child(Self::api_row(
                        "API Key",
                        self.secret(&self.polish_key, None, cx),
                    ))
                    .child(Self::api_row("模型 ID", Input::new(&self.polish_model)))
                    .child(Self::api_row("润色提示词", self.prompt_field(true, cx)))
                    .child(Self::api_row("测试", self.test_panel(3, cx)))
            });
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(recognition)
            .child(polish)
            .into_any_element()
    }
    pub(super) fn api_footer(&self, cx: &mut Context<Self>) -> Div {
        let dirty = self.dirty(cx);
        let valid = self.draft_api(cx).validate().is_ok();
        div()
            .px_7()
            .py_3()
            .border_t_1()
            .border_color(rgb(LINE))
            .flex()
            .items_center()
            .gap_2()
            .bg(rgb(PAPER))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(rgb(if dirty { 0xb18a50 } else { ACCENT }))
                    .child(if dirty {
                        "● 有未保存的更改"
                    } else if !valid {
                        "尚未配置完成"
                    } else if self.api.saved {
                        "✓ 已保存 · 下一次录音使用新配置"
                    } else {
                        "已保存"
                    }),
            )
            .child(
                Button::new("revert-api", "撤销更改")
                    .disabled(!dirty)
                    .on_click(cx.listener(|this, _, _, cx| this.revert_api(cx))),
            )
            .child(
                Button::new("save-api", "保存")
                    .variant(ButtonVariant::Primary)
                    .disabled(!dirty)
                    .on_click(cx.listener(|this, _, _, cx| this.save_api(cx))),
            )
    }
}

impl Drop for ApiState {
    fn drop(&mut self) {
        if let Some(job) = self.active.take() {
            let _ = job.cancel.try_send(());
        }
    }
}
