use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    io::Write,
    path::Path,
    ptr::{null, null_mut},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LPARAM, LRESULT, LocalFree, WPARAM,
    },
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
    Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

use crate::engine::Command;

pub static BUSY: AtomicBool = AtomicBool::new(false);
static HELD: AtomicBool = AtomicBool::new(false);
static ACCEPTED: AtomicBool = AtomicBool::new(false);
static COMMANDS: OnceLock<mpsc::Sender<Command>> = OnceLock::new();

pub struct Instance(HANDLE);
impl Instance {
    pub fn acquire() -> Result<Self> {
        let name: Vec<u16> = "Local\\ASRInput.PushToTalk\0".encode_utf16().collect();
        // SAFETY: name is NUL terminated and retained for the duration of the call.
        let handle = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
        ensure!(!handle.is_null(), "无法创建应用实例锁");
        // SAFETY: GetLastError is read immediately after CreateMutexW.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: this is our handle, not the other instance's handle.
            unsafe {
                CloseHandle(handle);
            }
            bail!("Ducky 已在运行，请从系统托盘打开；更新版本前请先退出旧程序");
        }
        Ok(Self(handle))
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: the owned handle is closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    window: usize,
    focus: usize,
    process: u32,
}

pub fn target() -> Target {
    // SAFETY: Windows fills only the supplied initialized structures.
    unsafe {
        let window = GetForegroundWindow();
        let mut process = 0;
        let thread = GetWindowThreadProcessId(window, &mut process);
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let focus = if GetGUIThreadInfo(thread, &mut info) != 0 {
            info.hwndFocus as usize
        } else {
            0
        };
        Target {
            window: window as usize,
            focus,
            process,
        }
    }
}

/// Installs a low-level hook on a dedicated message loop. Only physical Right Ctrl
/// is consumed; other keys and synthetic events pass through unchanged.
pub fn start_hook(commands: mpsc::Sender<Command>) -> Result<()> {
    COMMANDS
        .set(commands)
        .map_err(|_| anyhow::anyhow!("快捷键已经初始化"))?;
    let (ready, receive) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("right-ctrl".into())
        .spawn(move || {
            // SAFETY: callback is static and this thread runs its message loop until process exit.
            unsafe {
                let hook =
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), GetModuleHandleW(null()), 0);
                if hook.is_null() {
                    let _ = ready.send(Err(std::io::Error::last_os_error().to_string()));
                    return;
                }
                let _ = ready.send(Ok(()));
                let mut message = MSG::default();
                while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                UnhookWindowsHookEx(hook);
            }
        })?;
    receive
        .recv()?
        .map_err(|e| anyhow::anyhow!("注册右 Ctrl 失败：{e}"))
}

unsafe extern "system" fn keyboard(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: WH_KEYBOARD_LL supplies a KBDLLHOOKSTRUCT for non-negative codes.
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if event.vkCode == VK_RCONTROL as u32 && event.flags & LLKHF_INJECTED == 0 {
            let kind = wparam as u32;
            if (kind == WM_KEYDOWN || kind == WM_SYSKEYDOWN) && !HELD.swap(true, Ordering::SeqCst) {
                let accepted = !BUSY.swap(true, Ordering::SeqCst);
                ACCEPTED.store(accepted, Ordering::SeqCst);
                if accepted {
                    let command = Command::Press {
                        at: Instant::now(),
                        target: target(),
                    };
                    if COMMANDS.get().is_none_or(|tx| tx.send(command).is_err()) {
                        BUSY.store(false, Ordering::SeqCst);
                    }
                }
            } else if kind == WM_KEYUP || kind == WM_SYSKEYUP {
                HELD.store(false, Ordering::SeqCst);
                if ACCEPTED.swap(false, Ordering::SeqCst) {
                    let _ = COMMANDS
                        .get()
                        .map(|tx| tx.send(Command::Release { at: Instant::now() }));
                }
            }
            return 1;
        }
    }
    // SAFETY: forwards the original hook parameters unchanged.
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

pub fn copy(text: &str) -> Result<()> {
    arboard::Clipboard::new()
        .context("无法打开剪贴板")?
        .set_text(text)
        .context("无法写入剪贴板")
}

const RIME_TEXT_PATH: &str = r"D:\Documents\weasel\cut\text.txt";

fn modifiers_held() -> bool {
    // SAFETY: GetAsyncKeyState only reads the state of these virtual keys.
    unsafe {
        [
            VK_LCONTROL,
            VK_RCONTROL,
            VK_LSHIFT,
            VK_RSHIFT,
            VK_LMENU,
            VK_RMENU,
            VK_LWIN,
            VK_RWIN,
        ]
        .iter()
        .any(|key| GetAsyncKeyState(*key as i32) < 0)
    }
}

fn write_rime_text(path: &Path, text: &str) -> Result<()> {
    // Never overwrite an unconsumed result or a file belonging to another producer.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "无法创建 Rime 上屏文件 {}（如果文件已存在，请先处理）",
                path.display()
            )
        })?;
    if let Err(error) = file.write_all(text.as_bytes()).and_then(|_| file.flush()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error).context("写入 Rime 上屏文件失败");
    }
    Ok(())
}

pub fn insert(text: &str, original: Target) -> Result<&'static str> {
    if original.window == 0 || original.process == std::process::id() || target() != original {
        return Ok("焦点已改变或目标无效，未上屏；结果保存在语音历史中");
    }
    if modifiers_held() {
        return Ok("有修饰键仍被按住，未上屏；结果保存在语音历史中");
    }
    let path = Path::new(RIME_TEXT_PATH);
    write_rime_text(path, text)?;
    // Never activate or restore any window. Check again immediately before injecting.
    if target() != original || modifiers_held() {
        let _ = std::fs::remove_file(path);
        return Ok("焦点或按键状态已改变，未上屏；结果保存在语音历史中");
    }
    let key = |vk, flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let events = [
        key(VK_LCONTROL, 0),
        key(VK_LSHIFT, 0),
        key(VK_F12, 0),
        key(VK_F12, KEYEVENTF_KEYUP),
        key(VK_LSHIFT, KEYEVENTF_KEYUP),
        key(VK_LCONTROL, KEYEVENTF_KEYUP),
    ];
    // SAFETY: INPUT entries are initialized and no window activation is requested.
    let sent = unsafe {
        SendInput(
            events.len() as u32,
            events.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    if sent != events.len() as u32 {
        let cleanup = [
            key(VK_F12, KEYEVENTF_KEYUP),
            key(VK_LSHIFT, KEYEVENTF_KEYUP),
            key(VK_LCONTROL, KEYEVENTF_KEYUP),
        ];
        unsafe {
            SendInput(
                cleanup.len() as u32,
                cleanup.as_ptr(),
                size_of::<INPUT>() as i32,
            );
        }
        if sent < 3 {
            let _ = std::fs::remove_file(path);
        }
        bail!("系统未完整发送 Rime 快捷键；请检查上屏文件和语音历史");
    }
    Ok("已发送 Rime 上屏快捷键，请确认输入法已处理")
}

pub fn protect(text: &str) -> Result<String> {
    if text.is_empty() {
        return Ok(String::new());
    }
    Ok(STANDARD.encode(crypt(text.as_bytes(), false)?))
}

pub fn unprotect(text: &str) -> Result<String> {
    if text.is_empty() {
        return Ok(String::new());
    }
    let bytes = crypt(
        &STANDARD.decode(text).context("API Key 存储格式损坏")?,
        true,
    )?;
    String::from_utf8(bytes).context("API Key 解码失败")
}

fn crypt(bytes: &[u8], decrypt: bool) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: DPAPI only reads input; the output allocation is copied then freed with LocalFree.
    unsafe {
        let success = if decrypt {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptProtectData(
                &input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        ensure!(
            success != 0,
            "Windows 无法{} API Key（需要原 Windows 用户账户）",
            if decrypt { "解密" } else { "保护" }
        );
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData.cast());
        Ok(result)
    }
}

pub fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both paths are valid NUL-terminated wide strings for this call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn show_error(message: &str) {
    let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "Ducky\0".encode_utf16().collect();
    // SAFETY: the strings remain valid for the modal call.
    unsafe {
        MessageBoxW(
            null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rime_file_is_utf8_and_never_overwrites_pending_text() {
        let path = std::env::temp_dir().join(format!(
            "ducky-rime-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_rime_text(&path, "中文\nhello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), "中文\nhello".as_bytes());
        assert!(write_rime_text(&path, "replacement").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), "中文\nhello".as_bytes());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn keys_round_trip_through_windows_dpapi() {
        let protected = protect("test-key-中文").unwrap();
        assert!(!protected.contains("test-key"));
        assert_eq!(unprotect(&protected).unwrap(), "test-key-中文");
        assert!(unprotect("not base64!").is_err());
    }
}
