//! GIF playback in a Win32 layered window, independent of GPUI composition.
use crate::{config::AnimationConfig, engine::Event};
use anyhow::{Context, Result, ensure};
use image::{AnimationDecoder, ImageDecoder, RgbaImage, codecs::gif::GifDecoder};
use std::{
    io::{Cursor, Read},
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM},
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
};

const UPDATE: u32 = WM_APP + 25;
struct Gif {
    frames: Vec<(RgbaImage, u32)>,
}
impl Gif {
    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= 8 * 1024 * 1024, "GIF 文件不能超过 8 MiB");
        let decoder = GifDecoder::new(Cursor::new(bytes)).context("无法读取 GIF")?;
        let (w, h) = decoder.dimensions();
        ensure!(
            w > 0 && h > 0 && w <= 512 && h <= 512,
            "GIF 尺寸需要在 512×512 以内"
        );
        let mut frames = Vec::new();
        let mut total = 0;
        for frame in decoder.into_frames() {
            ensure!(frames.len() < 120, "GIF 最多支持 120 帧");
            let frame = frame.context("GIF 帧损坏")?;
            let (n, d) = frame.delay().numer_denom_ms();
            let delay = (n / d.max(1)).clamp(30, 1000);
            total += frame.buffer().len();
            ensure!(
                total <= 16 * 1024 * 1024,
                "GIF 解码后超过 16 MiB，请缩小或减少帧数"
            );
            frames.push((frame.into_buffer(), delay));
        }
        ensure!(!frames.is_empty(), "GIF 没有可播放帧");
        Ok(Self { frames })
    }
}

pub fn validate_gif_path(path: &str) -> Result<()> {
    load_custom(path).map(|_| ())
}
fn load_custom(path: &str) -> Result<Gif> {
    ensure!(!path.trim().is_empty(), "请选择 GIF 文件");
    let mut bytes = Vec::new();
    std::fs::File::open(path.trim())
        .context("无法打开 GIF 文件")?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    Gif::decode(&bytes)
}

enum Command {
    Show(AnimationConfig, bool, bool),
    Hide,
}
pub struct Overlay {
    hwnd: isize,
    tx: mpsc::Sender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
    drawn: Arc<AtomicUsize>,
}
impl Overlay {
    pub fn start(events: async_channel::Sender<Event>) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready, receive) = mpsc::sync_channel(1);
        let drawn = Arc::new(AtomicUsize::new(0));
        let counter = drawn.clone();
        let thread = std::thread::Builder::new()
            .name("gif-overlay".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let mut state = Box::new(State {
                        hwnd: null_mut(),
                        rx,
                        recording: Gif::decode(include_bytes!("../assets/recording.gif"))?,
                        processing: Gif::decode(include_bytes!("../assets/processing.gif"))?,
                        custom: None,
                        canvas: None,
                        frame: 0,
                        process: false,
                        deadline: None,
                        point: POINT::default(),
                        drawn: counter,
                        events,
                    });
                    let name: Vec<u16> = "ASRInput.GifOverlay\0".encode_utf16().collect();
                    // SAFETY: state stays boxed on this thread for the complete window lifetime.
                    unsafe {
                        let module = GetModuleHandleW(null());
                        let class = WNDCLASSW {
                            lpfnWndProc: Some(procedure),
                            hInstance: module,
                            lpszClassName: name.as_ptr(),
                            ..Default::default()
                        };
                        ensure!(RegisterClassW(&class) != 0, "无法注册 GIF 窗口");
                        state.hwnd = CreateWindowExW(
                            WS_EX_LAYERED
                                | WS_EX_TOPMOST
                                | WS_EX_TOOLWINDOW
                                | WS_EX_NOACTIVATE
                                | WS_EX_TRANSPARENT,
                            name.as_ptr(),
                            name.as_ptr(),
                            WS_POPUP,
                            0,
                            0,
                            168,
                            52,
                            null_mut(),
                            null_mut(),
                            module,
                            (&mut *state as *mut State).cast(),
                        );
                        ensure!(!state.hwnd.is_null(), "无法创建 GIF 窗口");
                        let _ = ready.send(Ok(state.hwnd as isize));
                        let mut message = MSG::default();
                        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                            TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = ready.send(Err(format!("{error:#}")));
                }
            })?;
        let hwnd = receive.recv()?.map_err(anyhow::Error::msg)?;
        Ok(Self {
            hwnd,
            tx,
            thread: Some(thread),
            drawn,
        })
    }
    pub fn event(&self, event: &Event, config: &AnimationConfig) {
        let command = match event {
            Event::AnimationError(_) => return,
            Event::Preview(settings) => Command::Show(settings.clone(), false, true),
            Event::Recording if config.enabled => Command::Show(config.clone(), false, false),
            Event::Recognizing | Event::Polishing if config.enabled && config.show_processing => {
                Command::Show(config.clone(), true, false)
            }
            _ => Command::Hide,
        };
        if self.tx.send(command).is_ok() {
            // SAFETY: hwnd belongs to this live GIF message thread.
            unsafe {
                PostMessageW(self.hwnd as HWND, UPDATE, 0, 0);
            }
        }
    }
    pub fn probe(&self) -> Probe {
        Probe {
            hwnd: self.hwnd,
            drawn: self.drawn.clone(),
        }
    }
}
#[derive(Clone)]
pub struct Probe {
    hwnd: isize,
    drawn: Arc<AtomicUsize>,
}
impl Probe {
    pub fn visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd as HWND) != 0 }
    }
    pub fn frames_drawn(&self) -> usize {
        self.drawn.load(Ordering::Relaxed)
    }
    pub fn validate(&self) -> bool {
        // SAFETY: read only the styles of our owned window.
        unsafe {
            let ex = GetWindowLongPtrW(self.hwnd as HWND, GWL_EXSTYLE) as u32;
            let style = GetWindowLongPtrW(self.hwnd as HWND, GWL_STYLE) as u32;
            ex & (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT)
                == (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT)
                && style & (WS_CAPTION | WS_THICKFRAME) == 0
                && ex & WS_EX_APPWINDOW == 0
                && GetForegroundWindow() != self.hwnd as HWND
        }
    }
}
impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: destruction occurs on the owning thread through WM_CLOSE.
        unsafe {
            PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    pixels: *mut u8,
    w: u32,
    h: u32,
}
impl Canvas {
    fn new(w: u32, h: u32) -> Result<Self> {
        // SAFETY: create a top-down 32-bit DIB, retained until Drop restores the old selection.
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            ensure!(!dc.is_null(), "无法创建 GIF 绘图上下文");
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
            if bitmap.is_null() {
                DeleteDC(dc);
                anyhow::bail!("无法创建 GIF 像素缓冲");
            }
            let old = SelectObject(dc, bitmap);
            Ok(Self {
                dc,
                bitmap,
                old,
                pixels: pixels.cast(),
                w,
                h,
            })
        }
    }
    fn draw(&mut self, hwnd: HWND, point: &POINT, frame: &RgbaImage) -> Result<()> {
        let resized =
            image::imageops::resize(frame, self.w, self.h, image::imageops::FilterType::Triangle);
        // SAFETY: the DIB allocation contains exactly w*h*4 writable bytes.
        unsafe {
            let pixels =
                std::slice::from_raw_parts_mut(self.pixels, (self.w * self.h * 4) as usize);
            for (dst, src) in pixels
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(resized.pixels())
            {
                let [r, g, b, a] = src.0;
                *dst = [premultiply(b, a), premultiply(g, a), premultiply(r, a), a];
            }
            let size = SIZE {
                cx: self.w as i32,
                cy: self.h as i32,
            };
            let source = POINT::default();
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            ensure!(
                UpdateLayeredWindow(
                    hwnd,
                    null_mut(),
                    point,
                    &size,
                    self.dc,
                    &source,
                    0,
                    &blend,
                    ULW_ALPHA
                ) != 0,
                "GIF 透明窗口更新失败"
            );
        }
        Ok(())
    }
}
fn premultiply(value: u8, alpha: u8) -> u8 {
    ((value as u16 * alpha as u16 + 127) / 255) as u8
}
impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

struct State {
    hwnd: HWND,
    rx: mpsc::Receiver<Command>,
    recording: Gif,
    processing: Gif,
    custom: Option<Gif>,
    canvas: Option<Canvas>,
    frame: usize,
    process: bool,
    deadline: Option<Instant>,
    point: POINT,
    drawn: Arc<AtomicUsize>,
    events: async_channel::Sender<Event>,
}
impl State {
    fn apply(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Hide => self.hide(),
            Command::Show(config, process, preview) => {
                self.hide();
                self.custom = None;
                if config.style == "custom" && !process {
                    self.custom = Some(load_custom(&config.gif_path)?);
                }
                self.process = process || config.style == "ribbon";
                self.frame = 0;
                self.drawn.store(0, Ordering::Relaxed);
                self.deadline = preview.then(|| Instant::now() + Duration::from_secs(3));
                let frame = &self.gif().frames[0].0;
                let (point, w, h) = placement(&config, frame.width(), frame.height());
                self.point = point;
                self.canvas = Some(Canvas::new(w, h)?);
                self.render()?;
                unsafe {
                    ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                }
            }
        }
        Ok(())
    }
    fn gif(&self) -> &Gif {
        if self.process {
            &self.processing
        } else {
            self.custom.as_ref().unwrap_or(&self.recording)
        }
    }
    fn render(&mut self) -> Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.hide();
            return Ok(());
        }
        let gif = if self.process {
            &self.processing
        } else {
            self.custom.as_ref().unwrap_or(&self.recording)
        };
        let (frame, delay) = &gif.frames[self.frame % gif.frames.len()];
        if let Some(canvas) = &mut self.canvas {
            canvas.draw(self.hwnd, &self.point, frame)?;
            self.drawn.fetch_add(1, Ordering::Relaxed);
        }
        self.frame += 1;
        unsafe {
            SetTimer(self.hwnd, 1, *delay, None);
        }
        Ok(())
    }
    fn hide(&mut self) {
        unsafe {
            KillTimer(self.hwnd, 1);
            ShowWindow(self.hwnd, SW_HIDE);
        }
        self.deadline = None;
        self.canvas = None;
    }
    fn error(&mut self, error: anyhow::Error) {
        self.hide();
        let _ = self
            .events
            .try_send(Event::AnimationError(format!("GIF 提示：{error:#}")));
    }
}

fn placement(config: &AnimationConfig, w: u32, h: u32) -> (POINT, u32, u32) {
    unsafe {
        let foreground = GetForegroundWindow();
        let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info);
        let dpi = GetDpiForWindow(foreground).max(96) as f32 / 96.0;
        let fit = (256.0 / w as f32).min(72.0 / h as f32).min(1.0);
        let scale = config.scale.clamp(0.75, 1.5) * dpi * fit;
        let (w, h) = (
            (w as f32 * scale).round().max(1.0) as u32,
            (h as f32 * scale).round().max(1.0) as u32,
        );
        let area = info.rcWork;
        let margin = (26.0 * dpi) as i32;
        let x = if config.position == "right" {
            area.right - w as i32 - margin
        } else {
            area.left + (area.right - area.left - w as i32) / 2
        };
        let y = if config.position == "top" {
            area.top + margin
        } else {
            area.bottom - h as i32 - margin
        };
        (
            POINT {
                x: x.max(area.left),
                y: y.max(area.top),
            },
            w,
            h,
        )
    }
}

unsafe extern "system" fn procedure(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: WM_NCCREATE receives our boxed State pointer; all subsequent access
    // happens on this single message thread before that box is dropped.
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lp as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            return 1;
        }
        // ShowWindow/UpdateLayeredWindow synchronously re-enter this procedure
        // for window-position and visibility messages. Do not borrow State for
        // those messages while apply/render already holds a mutable reference.
        match message {
            WM_NCHITTEST => return HTTRANSPARENT as isize,
            WM_MOUSEACTIVATE => return MA_NOACTIVATE as isize,
            WM_DESTROY => {
                PostQuitMessage(0);
                return 0;
            }
            UPDATE | WM_TIMER | WM_CLOSE => {}
            _ => return DefWindowProcW(hwnd, message, wp, lp),
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if !pointer.is_null() {
            let state = &mut *pointer;
            match message {
                UPDATE => {
                    while let Ok(command) = state.rx.try_recv() {
                        if let Err(error) = state.apply(command) {
                            state.error(error);
                        }
                    }
                    return 0;
                }
                WM_TIMER => {
                    if state.canvas.is_some()
                        && let Err(error) = state.render()
                    {
                        state.error(error);
                    }
                    return 0;
                }
                WM_CLOSE => {
                    state.hide();
                    DestroyWindow(hwnd);
                    return 0;
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, message, wp, lp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builtin_gif_has_distinct_frames_and_transparent_background() {
        let gif = Gif::decode(include_bytes!("../assets/recording.gif")).unwrap();
        assert_eq!(gif.frames.len(), 24);
        assert_eq!(gif.frames[0].0.get_pixel(0, 0).0[3], 0);
        assert_ne!(gif.frames[0].0.as_raw(), gif.frames[6].0.as_raw());
        assert!(
            gif.frames
                .iter()
                .all(|(image, delay)| image.dimensions() == (168, 52) && *delay == 50)
        );
    }
    #[test]
    fn gif_rejects_other_formats_and_alpha_is_premultiplied() {
        assert!(Gif::decode(b"not a gif").is_err());
        assert_eq!(premultiply(255, 128), 128);
        assert_eq!(premultiply(200, 0), 0);
    }
}
