//! Windows integration. Every HWND and clipboard handle is confined to the message thread.
use crate::model::*;
use anyhow::{Result, bail, ensure};
use std::{
    cell::RefCell,
    mem::size_of,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use windows_sys::Win32::{
    Foundation::*,
    Security::Credentials::*,
    System::{DataExchange::*, LibraryLoader::*, Memory::*, Registry::*, Threading::*},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
/// Only simplify presentation; retain the canonical extended path for all file operations.
pub fn display_path(path: &std::path::Path) -> String {
    use std::path::{Component, Prefix};
    let text = path.to_string_lossy();
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(_) => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
            Prefix::VerbatimUNC(_, _) => {
                format!(r"\\{}", text.strip_prefix(r"\\?\UNC\").unwrap_or(&text))
            }
            _ => text.into_owned(),
        },
        _ => text.into_owned(),
    }
}
const WAKE: u32 = WM_APP + 73;
const ACTIVATE: u32 = WM_APP + 74;
const TAG: usize = 0x53574350;
const TEXT: u32 = 13;
#[derive(Default, Debug)]
pub struct AltTap {
    down: bool,
    chord: bool,
}
#[derive(Default, Debug)]
pub struct AltDecision {
    pub swallow: bool,
    pub replay: bool,
    pub summon: bool,
}
impl AltTap {
    pub fn event(&mut self, vk: u32, down: bool, modifiers: bool) -> AltDecision {
        if vk == VK_RMENU as u32 {
            if down {
                if self.down {
                    return AltDecision {
                        swallow: !self.chord,
                        ..Default::default()
                    };
                }
                self.down = true;
                self.chord = modifiers;
                AltDecision {
                    swallow: !self.chord,
                    ..Default::default()
                }
            } else {
                let alone = self.down && !self.chord;
                self.down = false;
                self.chord = false;
                AltDecision {
                    swallow: alone,
                    summon: alone,
                    ..Default::default()
                }
            }
        } else if self.down && down && !self.chord {
            self.chord = true;
            AltDecision {
                replay: true,
                ..Default::default()
            }
        } else {
            AltDecision::default()
        }
    }
}
enum Request {
    Hotkey(String, oneshot::Sender<Result<()>>),
    Write(String, Option<u32>, oneshot::Sender<Result<u32>>),
    Baseline,
    Stop,
}
struct Context {
    backend: Backend,
    ui: UiSender,
    rx: mpsc::Receiver<Request>,
    listening: Arc<AtomicBool>,
    alt_on: Arc<AtomicBool>,
    max: Arc<AtomicUsize>,
    own_seq: u32,
    alt: AltTap,
    retries: u8,
    hotkey_id: i32,
    taskbar_msg: u32,
}
thread_local! {static CTX:RefCell<Option<Context>>=const{RefCell::new(None)};}
#[derive(Clone)]
pub struct Native {
    hotkey: Arc<Mutex<String>>,
    hwnd: isize,
    tx: mpsc::Sender<Request>,
    pub listening: Arc<AtomicBool>,
    pub alt_on: Arc<AtomicBool>,
    pub max: Arc<AtomicUsize>,
}
impl Native {
    pub fn start(backend: Backend, ui: UiSender, s: &Settings) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready, wait) = mpsc::sync_channel(1);
        let listening = Arc::new(AtomicBool::new(s.listening()));
        let alt_on = Arc::new(AtomicBool::new(s.right_alt));
        let max = Arc::new(AtomicUsize::new(s.max_text_bytes.max(s.max_sync_bytes)));
        let hotkey = Arc::new(Mutex::new(s.hotkey.clone()));
        let startup_hotkey = s.hotkey.clone();
        let context = Context {
            backend,
            ui,
            rx,
            listening: listening.clone(),
            alt_on: alt_on.clone(),
            max: max.clone(),
            own_seq: 0,
            alt: AltTap::default(),
            retries: 0,
            hotkey_id: 1,
            taskbar_msg: 0,
        };
        thread::Builder::new()
            .name("swiftclip-win32".into())
            .spawn(move || unsafe {
                // SAFETY: registration and all window operations run on this thread; class strings live through creation.
                let name = wide("SwiftClip.Native.v1");
                let instance = GetModuleHandleW(null());
                let class = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: instance,
                    lpszClassName: name.as_ptr(),
                    ..std::mem::zeroed()
                };
                RegisterClassW(&class);
                let mut context = context;
                context.taskbar_msg = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
                CTX.with(|c| *c.borrow_mut() = Some(context));
                let hwnd = CreateWindowExW(
                    0,
                    name.as_ptr(),
                    name.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    null_mut(),
                    null_mut(),
                    instance,
                    null(),
                );
                if hwnd.is_null() {
                    let _ = ready.send(Err(anyhow::anyhow!("无法创建剪贴板消息窗口")));
                    return;
                }
                if AddClipboardFormatListener(hwnd) == 0 {
                    let _ = ready.send(Err(anyhow::anyhow!("无法注册剪贴板监听")));
                    DestroyWindow(hwnd);
                    return;
                }
                let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), instance, 0);
                if hook.is_null() {
                    CTX.with(|c| {
                        if let Some(c) = c.borrow().as_ref() {
                            let _ = c.ui.unbounded_send(UiEvent::Error(
                                "右 Alt 监听注册失败，仍可从托盘打开".into(),
                            ));
                        }
                    });
                }
                let registered = parse_hotkey(&startup_hotkey).is_ok_and(|(mods, key)| {
                    RegisterHotKey(hwnd, 1, mods | MOD_NOREPEAT, key) != 0
                });
                if !registered {
                    CTX.with(|c| {
                        if let Some(c) = c.borrow().as_ref() {
                            let _ = c.ui.unbounded_send(UiEvent::Error(format!(
                                "备用快捷键 {startup_hotkey} 注册失败，请在设置中更换"
                            )));
                        }
                    });
                }
                let _ = ready.send(Ok(hwnd as isize));
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if !hook.is_null() {
                    UnhookWindowsHookEx(hook);
                }
                UnregisterHotKey(hwnd, 1);
                UnregisterHotKey(hwnd, 2);
                RemoveClipboardFormatListener(hwnd);
                DestroyWindow(hwnd);
            })?;
        Ok(Self {
            hwnd: wait.recv()??,
            hotkey,
            tx,
            listening,
            alt_on,
            max,
        })
    }
    pub async fn configure(&self, s: &Settings) -> Result<()> {
        let old = self
            .hotkey
            .lock()
            .map_err(|_| anyhow::anyhow!("快捷键状态不可用"))?
            .clone();
        if old != s.hotkey {
            let (tx, rx) = oneshot::channel();
            self.post(Request::Hotkey(s.hotkey.clone(), tx))?;
            rx.await??;
            *self
                .hotkey
                .lock()
                .map_err(|_| anyhow::anyhow!("快捷键状态不可用"))? = s.hotkey.clone();
        }
        self.listening.store(s.listening(), Ordering::Relaxed);
        self.alt_on.store(s.right_alt, Ordering::Relaxed);
        self.max
            .store(s.max_text_bytes.max(s.max_sync_bytes), Ordering::Relaxed);
        Ok(())
    }
    fn post(&self, r: Request) -> Result<()> {
        self.tx.send(r)?;
        unsafe {
            ensure!(
                PostMessageW(self.hwnd as HWND, WAKE, 0, 0) != 0,
                "无法通知剪贴板线程（Win32 {}）",
                GetLastError()
            );
        }
        Ok(())
    }
    pub async fn write(&self, text: String, expected: Option<u32>) -> Result<u32> {
        let (tx, rx) = oneshot::channel();
        self.post(Request::Write(text, expected, tx))?;
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .map_err(|_| anyhow::anyhow!("等待剪贴板写入确认超时，未自动粘贴"))??
    }
    pub fn baseline(&self) {
        let _ = self.post(Request::Baseline);
    }
    pub fn stop(&self) {
        let _ = self.post(Request::Stop);
    }
}
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: Windows invokes this callback for our message-thread window.
    unsafe {
        if CTX.with(|slot| slot.try_borrow().is_err()) {
            crate::diagnostics::record(
                "native_context_reentry",
                serde_json::json!({"message":format!("0x{msg:X}")}),
            );
        }
        let restart = CTX.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|c| c.taskbar_msg != 0 && msg == c.taskbar_msg)
        });
        if restart {
            CTX.with(|slot| {
                if let Some(c) = slot.borrow().as_ref() {
                    let _ = c.ui.unbounded_send(UiEvent::TrayRestart);
                }
            });
            return 0;
        }
        match msg {
            WM_CLIPBOARDUPDATE | WM_TIMER => {
                if msg == WM_TIMER {
                    KillTimer(hwnd, 1);
                }
                let capture = CTX.with(|slot| {
                    slot.borrow().as_ref().and_then(|c| {
                        c.listening
                            .load(Ordering::Relaxed)
                            .then_some((c.own_seq, c.max.load(Ordering::Relaxed)))
                    })
                });
                if let Some((own_seq, max)) = capture {
                    if GetClipboardSequenceNumber() == own_seq {
                        return 0;
                    }
                    // Delayed rendering can dispatch messages too. Never retain a CTX borrow across Win32 clipboard calls.
                    let result = read_clipboard(max);
                    let retry = CTX.with(|slot| {
                        let mut ctx = slot.borrow_mut();
                        let Some(c) = ctx.as_mut() else {
                            return false;
                        };
                        match result {
                            Ok(Some((seq, text))) => {
                                c.retries = 0;
                                if !c.backend.clipboard(seq, text, false) {
                                    let _ = c.ui.unbounded_send(UiEvent::Notice(
                                        "捕获队列已满，部分极快复制可能未记录".into(),
                                    ));
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                if !e.to_string().contains("占用") {
                                    c.retries = 0;
                                    let _ = c.ui.unbounded_send(UiEvent::Notice(format!(
                                        "已跳过剪贴板：{e}"
                                    )));
                                    return false;
                                }
                                if c.retries < 5 {
                                    c.retries += 1;
                                    return true;
                                } else {
                                    c.retries = 0;
                                    let _ = c.ui.unbounded_send(UiEvent::Notice(
                                        "剪贴板被占用，本次捕获未成功".into(),
                                    ));
                                }
                            }
                        }
                        false
                    });
                    if retry {
                        SetTimer(hwnd, 1, 25, None);
                    }
                }
                0
            }
            WAKE => {
                loop {
                    let req =
                        CTX.with(|slot| slot.borrow().as_ref().and_then(|c| c.rx.try_recv().ok()));
                    let Some(req) = req else {
                        break;
                    };
                    // EmptyClipboard synchronously sends WM_DESTROYCLIPBOARD to the previous owner,
                    // including this same window on the next paste. Release the borrow before calling it.
                    match req {
                        Request::Hotkey(key, done) => {
                            if done.is_closed() {
                                continue;
                            }
                            let result = (|| {
                                let (mods, vk) = parse_hotkey(&key)?;
                                let old = CTX
                                    .with(|slot| slot.borrow().as_ref().map_or(1, |c| c.hotkey_id));
                                let next = if old == 1 { 2 } else { 1 };
                                ensure!(
                                    RegisterHotKey(hwnd, next, mods | MOD_NOREPEAT, vk) != 0,
                                    "快捷键已被其他程序占用"
                                );
                                UnregisterHotKey(hwnd, old);
                                CTX.with(|slot| {
                                    if let Some(c) = slot.borrow_mut().as_mut() {
                                        c.hotkey_id = next;
                                    }
                                });
                                Ok(())
                            })();
                            let _ = done.send(result);
                        }
                        Request::Write(text, expected, done) => {
                            if done.is_closed() {
                                continue;
                            }
                            let result = write_clipboard(hwnd, &text, expected);
                            if let Err(error) = &result {
                                crate::diagnostics::record(
                                    "clipboard_write_failed",
                                    serde_json::json!({"error":error.to_string()}),
                                );
                            }
                            if let Ok(seq) = result {
                                CTX.with(|slot| {
                                    if let Some(c) = slot.borrow_mut().as_mut() {
                                        c.own_seq = seq;
                                    }
                                });
                            }
                            let _ = done.send(result);
                        }
                        Request::Baseline => {
                            let capture = CTX.with(|slot| {
                                slot.borrow().as_ref().and_then(|c| {
                                    c.listening
                                        .load(Ordering::Relaxed)
                                        .then(|| (c.max.load(Ordering::Relaxed), c.backend.clone()))
                                })
                            });
                            if let Some((max, backend)) = capture
                                && let Ok(Some((seq, text))) = read_clipboard(max)
                            {
                                let _ = backend.clipboard(seq, text, true);
                            }
                        }
                        Request::Stop => PostQuitMessage(0),
                    }
                }
                0
            }
            WM_HOTKEY | ACTIVATE => {
                let target = GetForegroundWindow() as isize;
                CTX.with(|slot| {
                    if let Some(c) = slot.borrow().as_ref() {
                        let _ = c.ui.unbounded_send(UiEvent::Summon(target));
                    }
                });
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}
unsafe extern "system" fn key_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: KBDLLHOOKSTRUCT is provided by the OS for this callback and not retained.
    unsafe {
        if code < 0 {
            return CallNextHookEx(null_mut(), code, w, l);
        }
        let k = &*(l as *const KBDLLHOOKSTRUCT);
        if k.flags & LLKHF_INJECTED != 0 || k.dwExtraInfo == TAG {
            return CallNextHookEx(null_mut(), code, w, l);
        }
        let down = w as u32 == WM_KEYDOWN || w as u32 == WM_SYSKEYDOWN;
        let mut decision = AltDecision::default();
        CTX.with(|slot| {
            if let Some(c) = slot.borrow_mut().as_mut() {
                if !c.alt_on.load(Ordering::Relaxed) {
                    return;
                }
                // An already-held character key is a chord too, not just Ctrl/Shift.
                let mods = k.vkCode == VK_RMENU as u32
                    && down
                    && (8..=255).any(|v| v != VK_RMENU as i32 && GetAsyncKeyState(v) < 0);
                decision = c.alt.event(k.vkCode, down, mods);
                if decision.summon {
                    let _ =
                        c.ui.unbounded_send(UiEvent::Summon(GetForegroundWindow() as isize));
                }
            }
        });
        if decision.replay {
            let i = key_input(VK_RMENU, KEYEVENTF_EXTENDEDKEY);
            SendInput(1, &i, size_of::<INPUT>() as i32);
        }
        if decision.swallow {
            1
        } else {
            CallNextHookEx(null_mut(), code, w, l)
        }
    }
}
unsafe fn read_clipboard(max: usize) -> Result<Option<(u32, String)>> {
    // SAFETY: clipboard is opened for the duration; borrowed global memory is copied before unlock/close.
    unsafe {
        if IsClipboardFormatAvailable(15) != 0 || IsClipboardFormatAvailable(TEXT) == 0 {
            return Ok(None);
        }
        ensure!(OpenClipboard(null_mut()) != 0, "剪贴板被占用");
        let result = (|| {
            let seq = GetClipboardSequenceNumber();
            let h = GetClipboardData(TEXT);
            ensure!(!h.is_null(), "无法读取剪贴板");
            let bytes = GlobalSize(h);
            ensure!(
                bytes <= max.saturating_mul(2).saturating_add(2),
                "文本超过大小上限"
            );
            let p = GlobalLock(h) as *const u16;
            ensure!(!p.is_null(), "无法锁定剪贴板");
            let units = std::slice::from_raw_parts(p, bytes / 2);
            let len = units.iter().position(|v| *v == 0).unwrap_or(units.len());
            let text = String::from_utf16(&units[..len]);
            GlobalUnlock(h);
            let text = text?;
            ensure!(seq == GetClipboardSequenceNumber(), "剪贴板已变化");
            if text.is_empty() || text.len() > max {
                Ok(None)
            } else {
                Ok(Some((seq, text)))
            }
        })();
        CloseClipboard();
        result
    }
}
unsafe fn write_clipboard(hwnd: HWND, text: &str, expected: Option<u32>) -> Result<u32> {
    // SAFETY: ownership of allocated memory transfers to Windows only after SetClipboardData succeeds.
    unsafe {
        ensure!(
            !text.contains('\0'),
            "正文含 NUL，无法完整写入 Windows 文本剪贴板"
        );
        crate::diagnostics::record(
            "clipboard_write_begin",
            serde_json::json!({"expected_sequence":expected,"current_sequence":GetClipboardSequenceNumber()}),
        );
        let data = wide(&windows_text(text));
        let h = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2);
        ensure!(!h.is_null(), "剪贴板分配失败");
        let p = GlobalLock(h) as *mut u16;
        if p.is_null() {
            GlobalFree(h);
            bail!("剪贴板内存锁定失败");
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        GlobalUnlock(h);
        for _ in 0..5 {
            if OpenClipboard(hwnd) != 0 {
                if expected.is_some_and(|s| s != GetClipboardSequenceNumber()) {
                    CloseClipboard();
                    GlobalFree(h);
                    bail!("本机剪贴板已更新，未覆盖");
                }
                if EmptyClipboard() == 0 {
                    let error = GetLastError();
                    CloseClipboard();
                    GlobalFree(h);
                    bail!("无法清空剪贴板（Win32 {error}）");
                }
                crate::diagnostics::record("clipboard_empty_complete", serde_json::json!({}));
                let result = SetClipboardData(TEXT, h);
                if result.is_null() {
                    let error = GetLastError();
                    CloseClipboard();
                    GlobalFree(h);
                    bail!("写入剪贴板失败（Win32 {error}）");
                }
                ensure!(
                    CloseClipboard() != 0,
                    "无法关闭剪贴板（Win32 {}）",
                    GetLastError()
                );
                let seq = GetClipboardSequenceNumber();
                crate::diagnostics::record(
                    "clipboard_write_confirmed",
                    serde_json::json!({"sequence":seq}),
                );
                return Ok(seq);
            }
            thread::sleep(Duration::from_millis(20));
        }
        GlobalFree(h);
        bail!("剪贴板被占用，请重试")
    }
}
fn key_input(vk: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: TAG,
            },
        },
    }
}
pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}
pub fn hide(hwnd: isize) {
    unsafe {
        crate::diagnostics::window("hide_before", hwnd);
        ShowWindow(hwnd as HWND, SW_HIDE);
        crate::diagnostics::window("hide_after", hwnd);
    }
}
pub fn show(hwnd: isize) {
    unsafe {
        crate::diagnostics::window("show_before", hwnd);
        ShowWindow(
            hwnd as HWND,
            if IsIconic(hwnd as HWND) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        SetForegroundWindow(hwnd as HWND);
    }
}
pub fn paste(target: PasteTarget, written_sequence: u32) -> Result<()> {
    // SAFETY: only injects the four Ctrl+V key events after verifying the original target still owns foreground.
    unsafe {
        let hwnd = target.hwnd as HWND;
        let mut pid = 0;
        let tid = GetWindowThreadProcessId(hwnd, &mut pid);
        ensure!(
            pid == target.process && tid == target.thread,
            "原应用窗口已变化，内容已复制"
        );
        ensure!(
            target.hwnd != 0 && IsWindow(hwnd) != 0,
            "原应用窗口已关闭，内容已复制"
        );
        crate::diagnostics::window("paste_before_restore", target.hwnd);
        if IsIconic(hwnd) != 0 {
            SetLastError(0);
            let restored = ShowWindowAsync(hwnd, SW_RESTORE);
            crate::diagnostics::record(
                "restore_minimized_requested",
                serde_json::json!({"returned":restored,"win32_error":GetLastError()}),
            );
        }
        crate::diagnostics::window("paste_after_restore", target.hwnd);
        if GetForegroundWindow() != hwnd {
            SetLastError(0);
            let activated = SetForegroundWindow(hwnd);
            let error = GetLastError();
            crate::diagnostics::record(
                "focus_restore_requested",
                serde_json::json!({"returned":activated,"win32_error":error}),
            );
        }
        let start = Instant::now();
        while [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN, VK_RETURN]
            .iter()
            .any(|v| GetAsyncKeyState(*v as i32) < 0)
        {
            ensure!(
                start.elapsed() < Duration::from_secs(2),
                "请松开修饰键后重试，内容已复制"
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(40));
        let focus_wait = Instant::now();
        while GetForegroundWindow() != hwnd && focus_wait.elapsed() < Duration::from_millis(200) {
            thread::sleep(Duration::from_millis(10));
        }
        ensure!(
            GetClipboardSequenceNumber() == written_sequence,
            "剪贴板已被其他操作更新，未发送粘贴"
        );
        ensure!(
            GetForegroundWindow() == hwnd,
            "焦点已变化，未发送粘贴；内容已复制"
        );
        let tid = GetWindowThreadProcessId(hwnd, &mut pid);
        ensure!(
            pid == target.process && tid == target.thread,
            "原应用窗口已变化，内容已复制"
        );
        let inputs = [
            key_input(VK_CONTROL, 0),
            key_input(b'V' as u16, 0),
            key_input(b'V' as u16, KEYEVENTF_KEYUP),
            key_input(VK_CONTROL, KEYEVENTF_KEYUP),
        ];
        crate::diagnostics::window("paste_before_input", target.hwnd);
        SetLastError(0);
        let sent = SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        );
        let input_error = GetLastError();
        crate::diagnostics::record(
            "send_input_result",
            serde_json::json!({"sent":sent,"requested":inputs.len(),"win32_error":input_error}),
        );
        if sent != inputs.len() as u32 {
            // Only release keys whose key-down was actually inserted. Do not leave V or Ctrl held.
            let mut release = Vec::new();
            if sent == 2 {
                release.push(key_input(b'V' as u16, KEYEVENTF_KEYUP));
            }
            if sent > 0 {
                release.push(key_input(VK_CONTROL, KEYEVENTF_KEYUP));
            }
            if !release.is_empty() {
                let released = SendInput(
                    release.len() as u32,
                    release.as_ptr(),
                    size_of::<INPUT>() as i32,
                );
                crate::diagnostics::record(
                    "send_input_cleanup",
                    serde_json::json!({"sent":released,"requested":release.len(),"win32_error":GetLastError()}),
                );
            }
            bail!("目标应用拒绝模拟输入（Win32 {input_error}），内容已复制");
        }
        Ok(())
    }
}
pub fn target(hwnd: isize) -> Option<PasteTarget> {
    unsafe {
        let mut process = 0;
        let thread = GetWindowThreadProcessId(hwnd as HWND, &mut process);
        if thread == 0 {
            None
        } else {
            Some(PasteTarget {
                hwnd,
                process,
                thread,
            })
        }
    }
}
pub fn restore_target(target: PasteTarget) {
    unsafe {
        let mut pid = 0;
        let tid = GetWindowThreadProcessId(target.hwnd as HWND, &mut pid);
        if pid == target.process && tid == target.thread {
            SetForegroundWindow(target.hwnd as HWND);
        }
    }
}
pub struct Instance(HANDLE);
impl Instance {
    pub fn acquire() -> Result<Option<Self>> {
        unsafe {
            let h = CreateMutexW(null(), 0, wide("Local\\SwiftClip.Rust.v1").as_ptr());
            ensure!(!h.is_null(), "无法创建单实例锁");
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                let hwnd = FindWindowW(wide("SwiftClip.Native.v1").as_ptr(), null());
                if !hwnd.is_null() {
                    PostMessageW(hwnd, ACTIVATE, 0, 0);
                }
                return Ok(None);
            }
            Ok(Some(Self(h)))
        }
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn credential_name(s: &Settings) -> Vec<u16> {
    wide(&format!(
        "SwiftClip-Rust/{}",
        hash(&format!(
            "{}\n{}",
            s.server_url.trim_end_matches('/'),
            s.username
        ))
    ))
}
pub fn password(s: &Settings) -> Result<String> {
    unsafe {
        let mut p = null_mut();
        if CredReadW(credential_name(s).as_ptr(), CRED_TYPE_GENERIC, 0, &mut p) == 0 {
            if GetLastError() == ERROR_NOT_FOUND {
                return Ok(String::new());
            }
            bail!("无法读取 Windows 凭据");
        }
        let bytes =
            std::slice::from_raw_parts((*p).CredentialBlob, (*p).CredentialBlobSize as usize);
        let result = String::from_utf8(bytes.to_vec());
        CredFree(p as _);
        Ok(result?)
    }
}
pub fn save_password(s: &Settings, password: &str) -> Result<()> {
    unsafe {
        let mut name = credential_name(s);
        let mut user = wide(&s.username);
        let mut blob = password.as_bytes().to_vec();
        let c = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: name.as_mut_ptr(),
            UserName: user.as_mut_ptr(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..std::mem::zeroed()
        };
        ensure!(CredWriteW(&c, 0) != 0, "无法保存 Windows 凭据");
        blob.fill(0);
        Ok(())
    }
}
pub fn startup(enabled: bool, _data_dir: &std::path::Path) -> Result<()> {
    unsafe {
        let mut h = null_mut();
        let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        ensure!(
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                0,
                null(),
                0,
                KEY_SET_VALUE,
                null(),
                &mut h,
                null_mut()
            ) == ERROR_SUCCESS,
            "无法打开开机自启设置"
        );
        let name = wide("Ducky-Clipboard");
        let result = if enabled {
            let exe = std::env::current_exe()?;
            let value = wide(&format!("{} --background", quote_path(&exe)));
            RegSetValueExW(
                h,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr() as _,
                (value.len() * 2) as u32,
            )
        } else {
            RegDeleteValueW(h, name.as_ptr())
        };
        RegCloseKey(h);
        ensure!(
            result == ERROR_SUCCESS || (!enabled && result == ERROR_FILE_NOT_FOUND),
            "修改开机自启失败"
        );
        Ok(())
    }
}
#[cfg(test)]
impl Native {
    pub fn test_stub() -> Self {
        let (tx, _) = mpsc::channel();
        Self {
            hwnd: 0,
            hotkey: Arc::new(Mutex::new("Ctrl+Alt+H".into())),
            tx,
            listening: Arc::new(AtomicBool::new(false)),
            alt_on: Arc::new(AtomicBool::new(false)),
            max: Arc::new(AtomicUsize::new(1048576)),
        }
    }
}

pub fn parse_hotkey(value: &str) -> Result<(u32, u32)> {
    let parts: Vec<_> = value.split('+').collect();
    let Some(last) = parts.last() else {
        bail!("快捷键格式无效")
    };
    let mut mods = 0;
    for p in &parts[..parts.len().saturating_sub(1)] {
        mods |= match p.to_ascii_lowercase().as_str() {
            "ctrl" => MOD_CONTROL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            _ => bail!("快捷键修饰键无效"),
        };
    }
    ensure!(
        mods != 0 && last.len() == 1 && last.as_bytes()[0].is_ascii_alphabetic(),
        "请选择带修饰键的字母快捷键"
    );
    Ok((mods, last.as_bytes()[0].to_ascii_uppercase() as u32))
}

/// CF_UNICODETEXT uses Windows line endings; stored and wire text remain exact.
pub fn windows_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn quote_path(path: &std::path::Path) -> String {
    let value = path.to_string_lossy();
    let trailing = value.chars().rev().take_while(|c| *c == '\\').count();
    format!("\"{}{}\"", value, "\\".repeat(trailing))
}
