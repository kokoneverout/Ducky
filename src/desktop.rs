use crate::engine::Event;
use anyhow::Result;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    ptr::{null, null_mut},
    sync::{
        OnceLock,
        atomic::{AtomicIsize, AtomicU32, Ordering},
        mpsc,
    },
    thread,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};

static MAIN: AtomicIsize = AtomicIsize::new(0);
static EVENTS: OnceLock<async_channel::Sender<Event>> = OnceLock::new();
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
const TRAY_MESSAGE: u32 = WM_APP + 10;

pub fn handle(window: &gpui::Window) -> HWND {
    match HasWindowHandle::window_handle(window)
        .expect("GPUI window has a native handle")
        .as_raw()
    {
        RawWindowHandle::Win32(handle) => handle.hwnd.get() as HWND,
        _ => unreachable!("ASRInput is a Windows app"),
    }
}

#[allow(clippy::manual_dangling_ptr)] // MAKEINTRESOURCEW(1) identifies an icon resource, not a dereferenced pointer.
pub fn prepare_main(window: &gpui::Window) {
    let hwnd = handle(window);
    MAIN.store(hwnd as isize, Ordering::SeqCst);
    // SAFETY: decorate only our HWND. Windows owns the caption buttons, moving,
    // maximize/restore and minimize behavior; do not replace GPUI's WNDPROC.
    unsafe {
        let caption: u32 = 0x00e2f5fb;
        let text: u32 = 0x004e584d;
        windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            hwnd,
            windows_sys::Win32::Graphics::Dwm::DWMWA_CAPTION_COLOR as u32,
            (&caption as *const u32).cast(),
            4,
        );
        windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            hwnd,
            windows_sys::Win32::Graphics::Dwm::DWMWA_TEXT_COLOR as u32,
            (&text as *const u32).cast(),
            4,
        );
        let module = GetModuleHandleW(null());
        for (kind, size) in [(ICON_BIG, 32), (ICON_SMALL, 16)] {
            let icon = LoadImageW(
                module,
                1usize as *const u16,
                IMAGE_ICON,
                size,
                size,
                LR_SHARED,
            );
            SendMessageW(hwnd, WM_SETICON, kind as usize, icon as isize);
        }
    }
}

pub fn hide_main() {
    // SAFETY: MAIN is installed from our live GPUI window; hiding does not destroy it.
    unsafe {
        ShowWindow(MAIN.load(Ordering::SeqCst) as HWND, SW_HIDE);
    }
}

pub fn show_main() {
    let hwnd = MAIN.load(Ordering::SeqCst) as HWND;
    if !hwnd.is_null() {
        // SAFETY: MAIN is the current main window. Restore before requesting focus.
        unsafe {
            ShowWindow(
                hwnd,
                if IsIconic(hwnd) != 0 {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            SetForegroundWindow(hwnd);
        }
    }
}

pub struct Tray {
    hwnd: isize,
    thread: Option<thread::JoinHandle<()>>,
}

impl Tray {
    pub fn start(events: async_channel::Sender<Event>) -> Result<Self> {
        EVENTS
            .set(events)
            .map_err(|_| anyhow::anyhow!("托盘已经初始化"))?;
        let (ready, receive) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("system-tray".into())
            .spawn(move || {
                // SAFETY: the class procedure and class name live for the process; the
                // hidden owner window is created and destroyed on this message thread.
                unsafe {
                    let name = wide("ASRInput.Tray");
                    let module = GetModuleHandleW(null());
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(tray_proc),
                        hInstance: module,
                        lpszClassName: name.as_ptr(),
                        ..Default::default()
                    };
                    if RegisterClassW(&class) == 0 {
                        let _ = ready.send(Err("注册托盘窗口失败"));
                        return;
                    }
                    TASKBAR_CREATED.store(
                        RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
                        Ordering::SeqCst,
                    );
                    let hwnd = CreateWindowExW(
                        WS_EX_TOOLWINDOW,
                        name.as_ptr(),
                        name.as_ptr(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        null_mut(),
                        null_mut(),
                        module,
                        null(),
                    );
                    if hwnd.is_null() {
                        let _ = ready.send(Err("创建托盘窗口失败"));
                        return;
                    }
                    if !notify(hwnd, NIM_ADD) {
                        DestroyWindow(hwnd);
                        let _ = ready.send(Err("创建系统托盘图标失败"));
                        return;
                    }
                    let _ = ready.send(Ok(hwnd as isize));
                    let mut message = MSG::default();
                    while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            })?;
        let hwnd = receive.recv()?.map_err(anyhow::Error::msg)?;
        Ok(Self {
            hwnd,
            thread: Some(thread),
        })
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: ask the owning thread to delete the tray icon and its window.
        unsafe {
            PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[allow(clippy::manual_dangling_ptr)] // Win32 integer resource identifier.
unsafe fn notify(hwnd: HWND, action: NOTIFY_ICON_MESSAGE) -> bool {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_MESSAGE,
        ..Default::default()
    };
    // SAFETY: icon 1 is embedded by build.rs; LR_SHARED owns its lifetime.
    data.hIcon = unsafe {
        LoadImageW(
            GetModuleHandleW(null()),
            1usize as *const u16,
            IMAGE_ICON,
            16,
            16,
            LR_SHARED,
        )
    };
    for (slot, value) in data
        .szTip
        .iter_mut()
        .zip(wide("Ducky · 按住右 Ctrl 说话"))
    {
        *slot = value;
    }
    // SAFETY: cbSize and fields match this NOTIFYICONDATAW layout.
    unsafe { Shell_NotifyIconW(action, &data) != 0 }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: this procedure receives messages only for our hidden tray owner.
    unsafe {
        if message != 0 && message == TASKBAR_CREATED.load(Ordering::SeqCst) {
            notify(hwnd, NIM_ADD);
            return 0;
        }
        match message {
            TRAY_MESSAGE if lp as u32 == WM_LBUTTONUP || lp as u32 == WM_LBUTTONDBLCLK => {
                show_main();
                return 0;
            }
            TRAY_MESSAGE if lp as u32 == WM_RBUTTONUP || lp as u32 == WM_CONTEXTMENU => {
                let menu = CreatePopupMenu();
                if menu.is_null() {
                    return 0;
                }
                AppendMenuW(menu, MF_STRING, 1, wide("打开 Ducky · 语音输入").as_ptr());
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
                AppendMenuW(menu, MF_STRING, 2, wide("退出").as_ptr());
                let mut point = POINT::default();
                GetCursorPos(&mut point);
                SetForegroundWindow(hwnd);
                let command = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                    point.x,
                    point.y,
                    0,
                    hwnd,
                    null(),
                );
                DestroyMenu(menu);
                PostMessageW(hwnd, WM_NULL, 0, 0);
                if command == 1 {
                    show_main();
                }
                if command == 2 {
                    let _ = EVENTS.get().map(|tx| tx.try_send(Event::Quit));
                }
                return 0;
            }
            WM_CLOSE => {
                notify(hwnd, NIM_DELETE);
                DestroyWindow(hwnd);
                return 0;
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return 0;
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, wp, lp)
    }
}

pub fn is_visible(window: &gpui::Window) -> bool {
    // SAFETY: visibility query for our own live window.
    unsafe { IsWindowVisible(handle(window)) != 0 }
}

pub fn system_command(window: &gpui::Window, command: u32) {
    // SAFETY: enqueue a standard command to our own main window.
    unsafe {
        PostMessageW(handle(window), WM_SYSCOMMAND, command as usize, 0);
    }
}
pub fn maximized(window: &gpui::Window) -> bool {
    unsafe { IsZoomed(handle(window)) != 0 }
}
pub fn minimized(window: &gpui::Window) -> bool {
    unsafe { IsIconic(handle(window)) != 0 }
}
pub fn caption_is_draggable(window: &gpui::Window) -> bool {
    // SAFETY: use a screen point in our native caption for a non-mutating hit test.
    unsafe {
        let hwnd = handle(window);
        let mut rect = windows_sys::Win32::Foundation::RECT::default();
        GetWindowRect(hwnd, &mut rect);
        let x = (rect.left + (rect.right - rect.left) / 2) as u16;
        let y = (rect.top + 20) as u16;
        SendMessageW(
            hwnd,
            WM_NCHITTEST,
            0,
            ((y as u32) << 16 | x as u32) as isize,
        ) == HTCAPTION as isize
    }
}
pub fn request_close(window: &gpui::Window) {
    unsafe {
        PostMessageW(handle(window), WM_CLOSE, 0, 0);
    }
}

pub fn choose_gif(window: &gpui::Window) -> Option<String> {
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    let mut buffer = vec![0u16; 32768];
    let filter: Vec<u16> = "GIF 动画\0*.gif\0\0".encode_utf16().collect();
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: handle(window),
        lpstrFilter: filter.as_ptr(),
        lpstrFile: buffer.as_mut_ptr(),
        nMaxFile: buffer.len() as u32,
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT,
        ..Default::default()
    };
    // SAFETY: buffers live for this synchronous Windows file dialog.
    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        return None;
    }
    let length = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..length]))
}
