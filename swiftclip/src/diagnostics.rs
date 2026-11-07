//! Paste diagnostics. Deliberately records no clipboard payload, preview, hash, title or key text.
use serde_json::{Value, json};
use std::{
    backtrace::Backtrace,
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};
use windows_sys::Win32::{
    Foundation::*,
    System::{Diagnostics::Debug::*, Threading::*},
    UI::WindowsAndMessaging::*,
};
static LOG: OnceLock<Mutex<File>> = OnceLock::new();
static OP: AtomicU64 = AtomicU64::new(0);
pub fn init(directory: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(directory)?;
    let file = directory.join("paste-diagnostic.jsonl");
    if std::fs::metadata(&file).is_ok_and(|m| m.len() > 4 * 1024 * 1024) {
        let _ = std::fs::rename(&file, directory.join("paste-diagnostic.previous.jsonl"));
    }
    let _ = LOG.set(Mutex::new(
        OpenOptions::new().create(true).append(true).open(file)?,
    ));
    std::panic::set_hook(Box::new(|info| {
        let class = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .map(|s| {
                if s.contains("already mutably borrowed") {
                    "refcell_already_mutably_borrowed"
                } else if s.contains("already borrowed") {
                    "refcell_already_borrowed"
                } else if s.contains("already being updated") {
                    "gpui_entity_reentrant_update"
                } else {
                    "payload_omitted"
                }
            })
            .unwrap_or("non_string_payload_omitted");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        record(
            "rust_panic",
            json!({"class":class,"location":location,"stack":Backtrace::force_capture().to_string()}),
        );
    }));
    unsafe {
        SetUnhandledExceptionFilter(Some(unhandled));
    }
    record(
        "process_start",
        json!({"version":env!("CARGO_PKG_VERSION")}),
    );
    Ok(())
}
pub fn begin(target: Option<crate::model::PasteTarget>) {
    OP.fetch_add(1, Ordering::Relaxed);
    record("confirm_requested", json!({}));
    if let Some(t) = target {
        window("confirm_target", t.hwnd);
    }
}
pub fn record(stage: &str, details: Value) {
    let event = json!({"time_utc_ms":crate::model::now(),"process":std::process::id(),"thread":unsafe{GetCurrentThreadId()},"operation":OP.load(Ordering::Relaxed),"stage":stage,"details":details});
    if let Some(log) = LOG.get()
        && let Ok(mut f) = log.lock()
    {
        let _ = serde_json::to_writer(&mut *f, &event);
        let _ = f.write_all(b"\n");
        let _ = f.flush();
    }
}
pub fn window(stage: &str, handle: isize) {
    record(stage, window_state(handle));
}
pub fn window_state(handle: isize) -> Value {
    unsafe {
        let hwnd = handle as HWND;
        let mut pid = 0;
        let tid = GetWindowThreadProcessId(hwnd, &mut pid);
        let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
        placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        let placement_ok = GetWindowPlacement(hwnd, &mut placement) != 0;
        let mut rect: RECT = std::mem::zeroed();
        let rect_ok = GetWindowRect(hwnd, &mut rect) != 0;
        let mut gui: GUITHREADINFO = std::mem::zeroed();
        gui.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        let gui_ok = tid != 0 && GetGUIThreadInfo(tid, &mut gui) != 0;
        let mut process_name = String::new();
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !process.is_null() {
            let mut buffer = [0u16; 1024];
            let mut len = buffer.len() as u32;
            if QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut len) != 0 {
                let p = String::from_utf16_lossy(&buffer[..len as usize]);
                process_name = p.rsplit(['\\', '/']).next().unwrap_or("").to_string();
            }
            CloseHandle(process);
        }
        json!({"hwnd":format!("0x{handle:X}"),"pid":pid,"process_name":process_name,"thread":tid,"valid":IsWindow(hwnd)!=0,"visible":IsWindowVisible(hwnd)!=0,"minimized":IsIconic(hwnd)!=0,"maximized":IsZoomed(hwnd)!=0,"placement_ok":placement_ok,"show_cmd":placement.showCmd,"rect_ok":rect_ok,"rect":[rect.left,rect.top,rect.right,rect.bottom],"foreground":format!("0x{:X}",GetForegroundWindow() as isize),"gui_ok":gui_ok,"focus":format!("0x{:X}",gui.hwndFocus as isize)})
    }
}
unsafe extern "system" fn unhandled(p: *const EXCEPTION_POINTERS) -> i32 {
    if !p.is_null() {
        unsafe {
            let e = (*p).ExceptionRecord;
            if !e.is_null() {
                record(
                    "native_exception",
                    json!({"code":format!("0x{:08X}",(*e).ExceptionCode as u32),"address":format!("0x{:X}",(*e).ExceptionAddress as usize),"stack":Backtrace::force_capture().to_string()}),
                );
            }
        }
    }
    EXCEPTION_CONTINUE_SEARCH
}
