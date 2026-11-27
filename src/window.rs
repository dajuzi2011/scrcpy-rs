//! Win32 窗口、渲染与输入事件（不依赖 winit，直接用系统 API）。
//!
//! 线程模型：
//! - 主线程：消息循环 + 绘制（Win32 窗口必须由创建它的线程处理消息）
//! - 解码线程：解码完成后把帧放进共享槽位并 SetEvent 唤醒主线程
//! - 主线程空闲时阻塞在 MsgWaitForMultipleObjectsEx 上，不做无谓的自旋

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, FrameRect, GetDC, GetMonitorInfoW, GetStockObject,
    ClientToScreen, InvalidateRect, MonitorFromWindow, ReleaseDC, ScreenToClient, SelectObject, SetBkMode,
    CreateDIBSection, GdiFlush, SetTextColor, TextOutW, UpdateWindow, BI_RGB, BITMAPINFO,
    BITMAPINFOHEADER, DEFAULT_GUI_FONT, DIB_RGB_COLORS, HBITMAP, HBRUSH, HDC, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, VK_CONTROL, VK_MENU, VK_SHIFT,
    TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::error::{Error, Result};
use crate::panel::{self, DumpStatus};
use crate::uitree::UiSnapshot;

/// 一帧已转换好的 BGRA 像素
pub struct FrameSurface {
    pub pixels: Vec<u32>,
    pub width: usize,
    pub height: usize,
    /// 这一帧被投递到共享槽的时刻（用来量「到达 → 上屏」的排队延迟）
    pub arrived: std::time::Instant,
}

/// 内存 DC 后备缓冲。
///
/// 之前的实现直接往窗口 DC 上「先用黑色 FillRect 清屏、再 StretchDIBits 贴图」，
/// 这两步之间窗口表面会短暂处于半成品状态，DWM 合成时就会被看到 —— 表现为满屏频闪。
/// 现在所有绘制都先落在内存位图上，最后只做一次 BitBlt，窗口上只可能看到完整的旧帧或新帧。
struct BackBuffer {
    mem_dc: HDC,
    bmp: HBITMAP,
    old_bmp: HGDIOBJ,
    w: i32,
    h: i32,
    /// 面板字体，按 DPI 建一次
    fonts: Option<panel::Fonts>,
    /// 诊断用累计耗时（微秒）
    us_video: u64,
    us_panel: u64,
    /// DIB 段的像素指针（32bpp 自上而下），我们自己往里写缩放结果
    bits: *mut u32,
    /// 每行像素数
    stride_px: usize,
}

// 位图指针只在本结构内使用，且整个结构由 Shared 里的 Mutex 串行访问
unsafe impl Send for BackBuffer {}

impl BackBuffer {
    fn new() -> Option<Self> {
        unsafe {
            let screen = GetDC(HWND::default());
            if screen.is_invalid() {
                return None;
            }
            let mem_dc = CreateCompatibleDC(screen);
            ReleaseDC(HWND::default(), screen);
            if mem_dc.is_invalid() {
                return None;
            }
            Some(Self {
                mem_dc,
                bmp: HBITMAP::default(),
                old_bmp: HGDIOBJ::default(),
                w: 0,
                h: 0,
                fonts: None,
                us_video: 0,
                us_panel: 0,
                bits: std::ptr::null_mut(),
                stride_px: 0,
            })
        }
    }

    /// 保证后备位图尺寸与客户区一致；尺寸变化时重建
    fn ensure(&mut self, cw: i32, ch: i32) -> bool {
        if cw <= 0 || ch <= 0 {
            return false;
        }
        if self.w == cw && self.h == ch && !self.bmp.is_invalid() {
            return true;
        }
        unsafe {
            let screen = GetDC(HWND::default());
            if screen.is_invalid() {
                return false;
            }
            ReleaseDC(HWND::default(), screen);

            // 用 DIB 段而不是兼容位图：拿到 CPU 可写的像素指针，缩放自己算，
            // 不再依赖 GDI 的 StretchDIBits（HALFTONE 太慢、最近邻有彩边）
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: cw,
                    biHeight: -ch, // 负数 = 自上而下，和我们的像素排布一致
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = match CreateDIBSection(
                self.mem_dc,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                None,
                0,
            ) {
                Ok(b) if !bits.is_null() => b,
                _ => return false,
            };
            let old = SelectObject(self.mem_dc, bmp);
            if !self.bmp.is_invalid() {
                let _ = DeleteObject(self.bmp);
            }
            self.bmp = bmp;
            self.old_bmp = old;
            self.bits = bits as *mut u32;
            self.stride_px = cw as usize;
            self.w = cw;
            self.h = ch;
            true
        }
    }

    fn render(
        &mut self,
        cw: i32,
        ch: i32,
        dpi: u32,
        panel_w: i32,
        smooth: bool,
        frame: Option<&FrameSurface>,
        overlay: Option<&UiSnapshot>,
        panel: Option<&panel::State>,
    ) -> u32 {
        let mut queue_us = 0u32;
        unsafe {
            // 黑边（letterbox）
            let rc = RECT {
                left: 0,
                top: 0,
                right: cw,
                bottom: ch,
            };
            let brush = CreateSolidBrush(COLORREF(0));
            FillRect(self.mem_dc, &rc, brush);
            let _ = DeleteObject(brush);

            if let Some(f) = frame {
                let (x, y, w, h) = video_rect(cw, ch, panel_w, f.width as i32, f.height as i32);
                let t_video = std::time::Instant::now();
                scale_into(
                    self.bits,
                    self.stride_px,
                    cw as usize,
                    ch as usize,
                    (x, y, w, h),
                    f,
                    smooth,
                );
                self.us_video += t_video.elapsed().as_micros() as u64;
                queue_us = f.arrived.elapsed().as_micros() as u32;
                // 叠加层画在视频之上、同一块离屏位图里，所以天然与画面同步
                if let Some(snap) = overlay {
                    draw_overlay(self.mem_dc, x, y, w, h, snap);
                }
            }

            if let Some(st) = panel {
                let t_panel = std::time::Instant::now();
                if self.fonts.is_none() {
                    self.fonts = Some(panel::Fonts::create(dpi));
                }
                if let Some(fonts) = self.fonts.as_ref() {
                    panel::draw(self.mem_dc, cw, ch, panel_w, fonts, st);
                }
                self.us_panel += t_panel.elapsed().as_micros() as u64;
            }
        }
        queue_us
    }

    fn blit_to(&self, dst: HDC, cw: i32, ch: i32) {
        unsafe {
            // 像素是我们用 CPU 直接写进 DIB 的，先让 GDI 把批处理刷下去再 BitBlt
            let _ = GdiFlush();
            let _ = BitBlt(dst, 0, 0, cw, ch, self.mem_dc, 0, 0, SRCCOPY);
        }
    }
}

impl Drop for BackBuffer {
    fn drop(&mut self) {
        unsafe {
            if !self.bmp.is_invalid() {
                SelectObject(self.mem_dc, self.old_bmp);
                let _ = DeleteObject(self.bmp);
            }
            if !self.mem_dc.is_invalid() {
                let _ = DeleteDC(self.mem_dc);
            }
        }
    }
}

/// 窗口把输入事件交给上层去编码成控制消息
pub trait InputSink: Send {
    fn on_touch(&mut self, action: u8, x: i32, y: i32, vw: u32, vh: u32, down: bool);
    fn on_scroll(&mut self, x: i32, y: i32, vw: u32, vh: u32, vscroll: f32);
    fn on_key(&mut self, action: u8, keycode: u32, meta: u32);
    fn on_text(&mut self, text: &str);
    fn on_command(&mut self, cmd: Command);
    /// 原始按键（UHID 键盘用）：vk 是 Windows 虚拟键，mods 是当前修饰键位
    fn on_key_raw(&mut self, _vk: u32, _down: bool, _mods: u8) {}
    /// 相对鼠标位移（UHID 鼠标用）
    fn on_mouse_move(&mut self, _dx: i32, _dy: i32) {}
    /// 鼠标按键状态变化（UHID 鼠标用，位掩码）
    fn on_mouse_button(&mut self, _buttons: u8) {}
    /// 鼠标滚轮（UHID 鼠标用）
    fn on_mouse_wheel(&mut self, _delta: i32) {}
    /// 文件拖放到窗口（路径列表）
    fn on_files(&mut self, _paths: &[String]) {}
    fn set_display_power(&mut self, _on: bool) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Back,
    Home,
    Power,
    ToggleScreen,
    Rotate,
    /// 开关「界面层级叠加」（uiautomator）
    ToggleOverlay,
    /// 保存当前帧为 PNG
    Screenshot,
    /// 开关录屏
    ToggleRecord,
    /// 打开截图/录像目录
    OpenFolder,
    /// 开关剪贴板同步
    ToggleClipboard,
    /// 把本机剪贴板粘贴到设备（服务端会顺带按 PASTE）
    PasteToDevice,
    /// 从设备取剪贴板：1=复制 2=剪切
    FetchClipboard(u8),
    /// 展开通知栏 / 展开快捷设置 / 收起面板
    ExpandNotification,
    ExpandSettings,
    CollapsePanels,
    /// 重置视频编码器（花屏时）
    ResetVideo,
    /// 启动 --start-app 指定的应用
    StartApp,
    /// 开关音频播放
    ToggleAudio,
    /// 用新的采集参数重连
    Reconnect,
    Quit,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 在客户区内按比例居中放置视频，返回 (x, y, w, h)
fn fit_rect(cw: i32, ch: i32, vw: i32, vh: i32) -> (i32, i32, i32, i32) {
    if vw <= 0 || vh <= 0 || cw <= 0 || ch <= 0 {
        return (0, 0, cw.max(1), ch.max(1));
    }
    let scale = (cw as f64 / vw as f64).min(ch as f64 / vh as f64);
    let w = (vw as f64 * scale).round() as i32;
    let h = (vh as f64 * scale).round() as i32;
    ((cw - w) / 2, (ch - h) / 2, w, h)
}

/// 视频在客户区里的位置：右侧让出控制面板，其余居中
fn video_rect(cw: i32, ch: i32, pw: i32, vw: i32, vh: i32) -> (i32, i32, i32, i32) {
    let content_w = (cw - pw).max(1);
    fit_rect(content_w, ch, vw, vh)
}

pub struct Shared {
    /// 解码线程放入的最新帧
    pending: Mutex<Option<FrameSurface>>,
    /// 主线程当前用于绘制的帧
    render: Mutex<Option<FrameSurface>>,
    /// 像素缓冲回收池：避免每帧都申请几 MB
    spare: Mutex<Option<Vec<u32>>>,
    /// 离屏后备缓冲（只在 UI 线程用）
    back: Mutex<Option<BackBuffer>>,
    /// 界面层级快照（由后台线程整体替换）
    overlay: Mutex<Option<Arc<UiSnapshot>>>,
    /// 叠加开关
    overlay_on: AtomicBool,
    /// 画面刷新上限（0 = 不限制），面板可调
    render_fps: AtomicU32,
    /// 是否正在录屏
    pub recording: AtomicBool,
    /// 剪贴板同步开关
    pub clipboard_on: AtomicBool,
    /// 音频播放开关
    pub audio_on: AtomicBool,
    /// 音频设备是否可用
    pub audio_ok: AtomicBool,
    /// 已生效的采集参数
    pub max_size: AtomicI32,
    pub bitrate: AtomicI32,
    pub max_fps: AtomicI32,
    /// 面板上待应用的采集参数
    pending_size: AtomicI32,
    pending_bitrate: AtomicI32,
    pending_fps: AtomicI32,
    /// 请求重连（UI 线程置位，run() 返回后由 app 层重建会话）
    pub reconnect_requested: AtomicBool,
    pub reconnect_busy: AtomicBool,
    /// 面板底部一行提示
    notice: Mutex<String>,
    /// 由设备侧写入本机剪贴板后，这里记一下新的序号，避免 watcher 又推回设备（回环）
    pub clip_guard: AtomicU32,
    /// UHID 键盘开关（模拟物理键盘）
    pub uhid_keyboard: AtomicBool,
    /// UHID 相对鼠标开关
    pub uhid_mouse: AtomicBool,
    /// 光标是否已被我们隐藏（避免 ShowCursor 计数不平衡）
    hidden_cursor: AtomicBool,
    /// 相对鼠标的按键掩码（左/右/中）
    mouse_mask: AtomicU8,
    /// 控制通道代数：每装入一个新的 Controller 就 +1。
    /// UHID 设备是 server 进程内的资源，重连后旧的全部消失，
    /// 客户端必须据此重建，否则设备端会一直报 "Unknown UHID id"。
    pub control_epoch: AtomicU32,
    /// 诊断用：UI 取到帧的次数
    pub frames_taken: AtomicU32,
    /// 诊断用：实际绘制次数
    pub paint_count: AtomicU32,
    /// 最近一秒实际绘制的次数（标题里显示，`0` 表示还没算出来）
    pub paint_fps: AtomicU32,
    /// 视频缩放是否用平滑采样（HALFTONE）。关掉更快，开着更干净
    pub smooth_scale: AtomicBool,
    /// 诊断用：最近一帧「投递 → 画出来」花了多少微秒
    pub queue_us: AtomicU32,
    /// 诊断用：累计绘制耗时（微秒）与次数
    pub paint_us: std::sync::atomic::AtomicU64,
    pub paint_timed: AtomicU32,
    /// 中文等非 ASCII 字符先攒在这里（IME 一次提交通常是连续多个 WM_CHAR），
    /// 攒够一小段再整段送出去。存 UTF-16 码元，代理对因此不会被拆散。
    text_buf: Mutex<Vec<u16>>,
    /// 视频流意外中断（解码线程置位，UI 循环看到后返回，交给上层重连）
    pub disconnected: AtomicBool,
    /// 会话状态：0 已连接 / 1 断开 / 2 重连中 / 3 致命
    pub session: AtomicU8,
    /// 重连倒计时（秒），0 表示没有在等待
    retry_in: AtomicU32,
    /// 面板上鼠标悬停的控件编码（0 = 无）
    hover: AtomicU32,
    /// 面板宽度（可拖动调整）
    panel_w: AtomicI32,
    /// 是否正在拖动面板分隔条
    resizing_panel: AtomicBool,
    /// 鼠标是否停在分隔条上
    handle_hot: AtomicBool,
    /// 最近一次抓取结果（面板显示）
    dump_status: Mutex<DumpStatus>,
    /// 有新帧时被 SetEvent
    frame_event: HANDLE,
    input: Mutex<Option<Box<dyn InputSink>>>,
    video_w: AtomicI32,
    video_h: AtomicI32,
    /// 鼠标左键是否处于按下拖动状态（决定要不要发 ACTION_UP）
    dragging: AtomicBool,
    fullscreen: AtomicBool,
    screen_off: AtomicBool,
    pub quit: AtomicBool,
    hwnd: AtomicIsize,
    title: Mutex<String>,
}

// HANDLE 只是内核对象句柄，SetEvent 本身线程安全
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    /// 解码线程调用：投递一帧并唤醒 UI 线程
    pub fn push_frame(&self, f: FrameSurface) {
        {
            let mut slot = self.pending.lock().unwrap();
            *slot = Some(f);
        }
        unsafe {
            let _ = SetEvent(self.frame_event);
        }
    }

    /// 取一个可复用的像素缓冲
    pub fn take_spare(&self) -> Option<Vec<u32>> {
        self.spare.lock().ok().and_then(|mut g| g.take())
    }

    fn put_spare(&self, mut v: Vec<u32>) {
        if let Ok(mut g) = self.spare.lock() {
            if g.is_none() {
                v.clear();
                *g = Some(v);
            }
        }
    }

    pub fn set_input(&self, sink: Box<dyn InputSink>) {
        if let Ok(mut g) = self.input.lock() {
            *g = Some(sink);
        }
    }

    // ---- 界面层级叠加 ----

    /// 整体替换当前快照；传 None 即清理（关闭开关时调用）
    pub fn set_overlay(&self, snap: Option<Arc<UiSnapshot>>) {
        if let Ok(mut g) = self.overlay.lock() {
            *g = snap;
        }
    }

    pub fn overlay(&self) -> Option<Arc<UiSnapshot>> {
        self.overlay.lock().ok().and_then(|g| g.clone())
    }

    pub fn overlay_enabled(&self) -> bool {
        self.overlay_on.load(Ordering::Relaxed)
    }

    pub fn set_overlay_enabled(&self, on: bool) {
        self.overlay_on.store(on, Ordering::Relaxed);
        if !on {
            // 关闭时立刻丢掉快照，避免残留框线
            self.set_overlay(None);
        }
    }

    /// 画面刷新上限（每秒帧数，0 = 不限制）
    pub fn render_fps(&self) -> u32 {
        self.render_fps.load(Ordering::Relaxed)
    }

    pub fn set_render_fps(&self, fps: u32) {
        self.render_fps.store(fps, Ordering::Relaxed);
    }

    /// 当前客户区下的面板宽度（0 表示不显示）
    pub fn panel_width(&self, cw: i32) -> i32 {
        panel::resolve_width(cw, self.panel_w.load(Ordering::Relaxed))
    }

    fn set_handle_hot(&self, hot: bool) {
        self.handle_hot.store(hot, Ordering::Relaxed);
    }

    fn handle_hot(&self) -> bool {
        self.handle_hot.load(Ordering::Relaxed)
    }

    fn set_panel_width(&self, w: i32) {
        self.panel_w.store(w, Ordering::Relaxed);
    }

    fn hover(&self) -> Option<panel::Item> {
        panel::Item::from_code(self.hover.load(Ordering::Relaxed))
    }

    fn set_hover(&self, item: Option<panel::Item>) {
        let code = item.map(|i| i.code()).unwrap_or(0);
        self.hover.store(code, Ordering::Relaxed);
    }

    pub fn dump_status(&self) -> DumpStatus {
        self.dump_status.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn set_dump_status(&self, st: DumpStatus) {
        if let Ok(mut g) = self.dump_status.lock() {
            *g = st;
        }
    }

    /// 装好新一代控制通道：代数 +1，让 UHID / 鼠标按键等会话内状态全部作废
    pub fn bump_control_epoch(&self) -> u32 {
        let e = self.control_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        self.request_repaint();
        e
    }

    pub fn control_epoch(&self) -> u32 {
        self.control_epoch.load(Ordering::Relaxed)
    }

    /// 清掉相对鼠标的按键掩码（换会话后旧的按下状态不该延续）
    pub fn reset_mouse_mask(&self) {
        self.mouse_mask.store(0, Ordering::Relaxed);
    }

    /// 取出攒下的非 ASCII 文本（并清空缓冲）
    pub fn take_text(&self) -> String {
        let Ok(mut g) = self.text_buf.lock() else {
            return String::new();
        };
        if g.is_empty() {
            return String::new();
        }
        let text = String::from_utf16_lossy(&g);
        g.clear();
        text
    }

    /// 标记会话已连接
    pub fn set_connected(&self) {
        self.session.store(0, Ordering::Relaxed);
        self.retry_in.store(0, Ordering::Relaxed);
        self.request_repaint();
    }

    /// 标记断线 / 正在重连（`retry_in` 为剩余秒数，0 表示正在立即重试）
    pub fn set_disconnected(&self, retry_in: u32) {
        self.session
            .store(if retry_in > 0 { 2 } else { 1 }, Ordering::Relaxed);
        self.retry_in.store(retry_in, Ordering::Relaxed);
        self.request_repaint();
    }

    /// 面板上显示的连接状态
    pub fn session_text(&self) -> String {
        match self.session.load(Ordering::Relaxed) {
            0 => "已连接".to_string(),
            1 => "连接已断开，正在重试…".to_string(),
            2 => {
                let n = self.retry_in.load(Ordering::Relaxed);
                if n > 0 {
                    format!("连接已断开，{n} 秒后重试")
                } else {
                    "连接已断开，正在重试…".to_string()
                }
            }
            _ => "连接失败".to_string(),
        }
    }

    /// 把当前状态打包给面板绘制/命中测试用
    pub fn panel_state(&self) -> panel::State {
        panel::State {
            overlay_on: self.overlay_enabled(),
            recording: self.recording.load(Ordering::Relaxed),
            clipboard_on: self.clipboard_on.load(Ordering::Relaxed),
            audio_on: self.audio_on.load(Ordering::Relaxed),
            fps_limit: self.render_fps(),
            max_size: self.max_size.load(Ordering::Relaxed).max(0) as u32,
            bitrate: self.bitrate.load(Ordering::Relaxed).max(0) as u32,
            max_fps: self.max_fps.load(Ordering::Relaxed).max(0) as u32,
            pending_size: self.pending_size.load(Ordering::Relaxed).max(0) as u32,
            pending_bitrate: self.pending_bitrate.load(Ordering::Relaxed).max(0) as u32,
            pending_fps: self.pending_fps.load(Ordering::Relaxed).max(0) as u32,
            reconnect_busy: self.reconnect_busy.load(Ordering::Relaxed),
            hover: self.hover(),
            handle_hot: self.handle_hot(),
            notice: self.notice.lock().map(|g| g.clone()).unwrap_or_default(),
            session: self.session_text(),
            smooth_scale: self.smooth_scale.load(Ordering::Relaxed),
            uhid_keyboard: self.uhid_keyboard.load(Ordering::Relaxed),
            uhid_mouse: self.uhid_mouse.load(Ordering::Relaxed),
            dump: self.dump_status(),
        }
    }

    /// 复制一份当前上屏的帧（截图用）
    pub fn render_snapshot(&self) -> Option<FrameSurface> {
        self.render.lock().ok().and_then(|g| {
            g.as_ref().map(|f| FrameSurface {
                pixels: f.pixels.clone(),
                width: f.width,
                height: f.height,
                arrived: std::time::Instant::now(),
            })
        })
    }

    /// 面板底部提示
    pub fn set_notice(&self, text: impl Into<String>) {
        if let Ok(mut g) = self.notice.lock() {
            *g = text.into();
        }
        self.request_repaint();
    }

    /// 记录当前会话已生效的采集参数（重连后由 app 层写入）
    pub fn set_active_params(&self, size: u32, bitrate: u32, fps: u32) {
        self.max_size.store(size as i32, Ordering::Relaxed);
        self.bitrate.store(bitrate as i32, Ordering::Relaxed);
        self.max_fps.store(fps as i32, Ordering::Relaxed);
        self.pending_size.store(size as i32, Ordering::Relaxed);
        self.pending_bitrate.store(bitrate as i32, Ordering::Relaxed);
        self.pending_fps.store(fps as i32, Ordering::Relaxed);
    }

    /// 面板上的「待应用帧率」（积压自愈时用来把采集帧率降下来）
    pub fn set_pending_fps(&self, fps: u32) {
        self.pending_fps.store(fps as i32, Ordering::Relaxed);
        self.request_repaint();
    }

    /// 待应用的参数是否需要重连
    pub fn params_dirty(&self) -> bool {
        self.pending_size.load(Ordering::Relaxed) != self.max_size.load(Ordering::Relaxed)
            || self.pending_bitrate.load(Ordering::Relaxed) != self.bitrate.load(Ordering::Relaxed)
            || self.pending_fps.load(Ordering::Relaxed) != self.max_fps.load(Ordering::Relaxed)
    }

    /// 让 UI 线程重画（可以从任意线程调用）
    pub fn request_repaint(&self) {
        let h = self.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            unsafe {
                let _ = InvalidateRect(HWND(h as *mut core::ffi::c_void), None, false);
            }
        }
    }

    fn with_input<F: FnOnce(&mut dyn InputSink)>(&self, f: F) {
        if let Ok(mut g) = self.input.lock() {
            if let Some(sink) = g.as_mut() {
                f(sink.as_mut());
            }
        }
    }

    pub fn set_video_size(&self, w: u32, h: u32) {
        self.video_w.store(w as i32, Ordering::Relaxed);
        self.video_h.store(h as i32, Ordering::Relaxed);
    }

    pub fn video_size(&self) -> (u32, u32) {
        (
            self.video_w.load(Ordering::Relaxed).max(0) as u32,
            self.video_h.load(Ordering::Relaxed).max(0) as u32,
        )
    }

    pub fn is_screen_off(&self) -> bool {
        self.screen_off.load(Ordering::Relaxed)
    }

    pub fn set_screen_off(&self, v: bool) {
        self.screen_off.store(v, Ordering::Relaxed);
    }

    /// 供上层在需要时（例如息屏）主动发一条控制消息
    pub fn with_sink<F: FnOnce(&mut dyn InputSink)>(&self, f: F) {
        self.with_input(f);
    }

    /// 任意线程都能改标题（用于显示实时帧率）
    pub fn set_title(&self, title: &str) {
        {
            let mut t = self.title.lock().unwrap();
            if *t == title {
                return;
            }
            *t = title.to_string();
        }
        let h = self.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            let w = wide(title);
            unsafe {
                let _ = SetWindowTextW(HWND(h as *mut core::ffi::c_void), PCWSTR(w.as_ptr()));
            }
        }
    }
}

pub struct Window {
    hwnd: HWND,
    shared: Arc<Shared>,
}

impl Window {
    /// 创建窗口。vw/vh 仅决定初始大小，后续跟视频流走
    pub fn create(title: &str, vw: u32, vh: u32, fullscreen: bool) -> Result<Window> {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

            let event = CreateEventW(None, false, false, PCWSTR::null())?;
            let shared = Arc::new(Shared {
                pending: Mutex::new(None),
                render: Mutex::new(None),
                spare: Mutex::new(None),
                back: Mutex::new(None),
                overlay: Mutex::new(None),
                overlay_on: AtomicBool::new(false),
                render_fps: AtomicU32::new(0),
                recording: AtomicBool::new(false),
                clipboard_on: AtomicBool::new(false),
                audio_on: AtomicBool::new(true),
                audio_ok: AtomicBool::new(false),
                max_size: AtomicI32::new(0),
                bitrate: AtomicI32::new(8_000_000),
                max_fps: AtomicI32::new(0),
                pending_size: AtomicI32::new(0),
                pending_bitrate: AtomicI32::new(8_000_000),
                pending_fps: AtomicI32::new(0),
                reconnect_requested: AtomicBool::new(false),
                reconnect_busy: AtomicBool::new(false),
                notice: Mutex::new(String::new()),
                clip_guard: AtomicU32::new(0),
                uhid_keyboard: AtomicBool::new(false),
                uhid_mouse: AtomicBool::new(false),
                hidden_cursor: AtomicBool::new(false),
                mouse_mask: AtomicU8::new(0),
                control_epoch: AtomicU32::new(0),
                text_buf: Mutex::new(Vec::new()),
                frames_taken: AtomicU32::new(0),
                paint_count: AtomicU32::new(0),
                paint_fps: AtomicU32::new(0),
                smooth_scale: AtomicBool::new(true),
                queue_us: AtomicU32::new(0),
                paint_us: std::sync::atomic::AtomicU64::new(0),
                paint_timed: AtomicU32::new(0),
                disconnected: AtomicBool::new(false),
                session: AtomicU8::new(0),
                retry_in: AtomicU32::new(0),
                hover: AtomicU32::new(0),
                panel_w: AtomicI32::new(panel::DEFAULT_W),
                resizing_panel: AtomicBool::new(false),
                handle_hot: AtomicBool::new(false),
                dump_status: Mutex::new(DumpStatus::default()),
                frame_event: event,
                input: Mutex::new(None),
                video_w: AtomicI32::new(vw as i32),
                video_h: AtomicI32::new(vh as i32),
                dragging: AtomicBool::new(false),
                fullscreen: AtomicBool::new(false),
                screen_off: AtomicBool::new(false),
                quit: AtomicBool::new(false),
                hwnd: AtomicIsize::new(0),
                title: Mutex::new(title.to_string()),
            });

            let hinstance = GetModuleHandleW(None)?;
            let class_name = wide("ScrcpyRustWindow");
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: HINSTANCE::from(hinstance),
                hIcon: HICON::default(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
            };
            let atom = RegisterClassW(&wc);
            if atom == 0 {
                let err = windows::core::Error::from_win32();
                // 1410 = 类已注册（重复创建时正常）
                if err.code().0 as u32 != 1410 {
                    return Err(Error::Win(err));
                }
            }

            // 初始窗口大小：不超过屏幕工作区的 80%
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);
            let max_w = (sw as f64 * 0.8).max(400.0);
            let max_h = (sh as f64 * 0.8).max(300.0);
            let scale = (max_w / vw.max(1) as f64).min(max_h / vh.max(1) as f64);
            let cw = ((vw.max(1) as f64 * scale) as i32).clamp(320, max_w as i32);
            let ch = ((vh.max(1) as f64 * scale) as i32).clamp(240, max_h as i32);
            // 控制面板占右侧固定宽度，客户区要加上它，否则画面会被压窄
            let cw = cw + panel::resolve_width(cw + panel::DEFAULT_W, panel::DEFAULT_W);

            let mut rect = RECT {
                left: 0,
                top: 0,
                right: cw,
                bottom: ch,
            };
            let style = WS_OVERLAPPEDWINDOW | WS_VISIBLE;
            let _ = AdjustWindowRect(&mut rect, style, false);

            let title_w = wide(title);
            let raw = Arc::into_raw(shared.clone()) as *const core::ffi::c_void;
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title_w.as_ptr()),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                HWND::default(),
                HMENU::default(),
                HINSTANCE::from(hinstance),
                Some(raw),
            )?;

            shared.hwnd.store(hwnd.0 as isize, Ordering::Relaxed);
            // 允许把文件拖进窗口（推送到设备）
            DragAcceptFiles(hwnd, true);
            let win = Window { hwnd, shared };
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = UpdateWindow(hwnd);
            if fullscreen {
                toggle_fullscreen_raw(hwnd, &win.shared);
            }
            Ok(win)
        }
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    pub fn set_title(&self, title: &str) {
        self.shared.set_title(title);
    }

    /// 进入消息循环，直到窗口关闭
    pub fn run(&self) -> Result<()> {
        let shared = self.shared.clone();
        loop {
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return Ok(());
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            if shared.quit.load(Ordering::Relaxed) {
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                }
                continue;
            }

            // 每秒结算一次「真实上屏帧率」。标题里的 @xx fps 是解码帧率，
            // 两者差得远就说明瓶颈在绘制而不是在解码/网络。
            {
                static LAST_REPORT: Mutex<Option<(std::time::Instant, u32, u32)>> =
                    Mutex::new(None);
                let now = std::time::Instant::now();
                let ft = shared.frames_taken.load(Ordering::Relaxed);
                let pc = shared.paint_count.load(Ordering::Relaxed);
                if let Ok(mut g) = LAST_REPORT.lock() {
                    if let Some((t, lf, lp)) = *g {
                        let secs = now.duration_since(t).as_secs_f32();
                        if secs >= 1.0 {
                            shared
                                .paint_fps
                                .store(((pc - lp) as f32 / secs).round() as u32, Ordering::Relaxed);
                            if debug_on() {
                                let us = shared.paint_us.swap(0, Ordering::Relaxed);
                                let n = shared.paint_timed.swap(0, Ordering::Relaxed);
                                let avg = if n > 0 { us / n as u64 / 1000 } else { 0 };
                                println!(
                                    "[ui] {secs:.1}s 内取帧 {} 绘制 {} 平均 {avg} ms/帧，队列 {} ms",
                                    ft - lf,
                                    pc - lp,
                                    shared.queue_us.load(Ordering::Relaxed) / 1000
                                );
                            }
                            *g = Some((now, ft, pc));
                        }
                    } else {
                        *g = Some((now, ft, pc));
                    }
                }
            }

            // 上层请求重建会话（改采集参数）：交给 app 层处理，窗口本身不销毁
            if shared.reconnect_requested.load(Ordering::Relaxed) {
                return Ok(());
            }
            // 视频流意外中断（拔线/设备端退出）：同样交回上层做退避重连
            if shared.disconnected.load(Ordering::Relaxed) {
                return Ok(());
            }

            // 等待新帧或新消息，最长 50ms（保证 quit 标志能被及时检查）
            let handles = [shared.frame_event];
            let r = unsafe {
                MsgWaitForMultipleObjectsEx(Some(&handles), 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
            };
            if r == WAIT_OBJECT_0 {
                let frame = {
                    let mut slot = shared.pending.lock().unwrap();
                    slot.take()
                };
                if let Some(f) = frame {
                    shared.frames_taken.fetch_add(1, Ordering::Relaxed);
                    let old = {
                        let mut render = shared.render.lock().unwrap();
                        render.replace(f)
                    };
                    if let Some(old) = old {
                        shared.put_spare(old.pixels);
                    }
                    unsafe {
                        let _ = InvalidateRect(self.hwnd, None, false);
                    }
                }
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let shared_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Shared;

    match msg {
        WM_CREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            LRESULT(0)
        }
        WM_DESTROY => {
            if !shared_ptr.is_null() {
                let arc = Arc::from_raw(shared_ptr);
                arc.quit.store(true, Ordering::Relaxed);
                drop(arc);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ if shared_ptr.is_null() => DefWindowProcW(hwnd, msg, wparam, lparam),
        _ => {
            let shared = &*shared_ptr;
            handle_message(hwnd, msg, wparam, lparam, shared)
        }
    }
}

fn paint(hwnd: HWND, shared: &Shared) {
    shared.paint_count.fetch_add(1, Ordering::Relaxed);
    let t0 = std::time::Instant::now();
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);

        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let cw = rc.right - rc.left;
        let ch = rc.bottom - rc.top;
        let pw = shared.panel_width(cw);

        // 排障用：SCRCPY_RS_DEBUG=1 时把客户区与面板几何打一次，便于脚本/人工核对坐标
        if debug_on() {
            static LAST: AtomicI32 = AtomicI32::new(0);
            let key = cw * 10_000_000 + ch * 1_000 + pw;
            if LAST.swap(key, Ordering::Relaxed) != key {
                println!(
                    "[dbg] 客户区 {cw}x{ch} 面板宽 {} 面板起点 x={} 视频区 {:?}",
                    pw,
                    panel::origin_x(cw, pw),
                    video_rect(
                        cw,
                        ch,
                        pw,
                        shared.video_size().0 as i32,
                        shared.video_size().1 as i32
                    )
                );
            }
        }
        let state = shared.panel_state();
        let panel_state = if pw > 0 { Some(&state) } else { None };
        let dpi = GetDpiForWindow(hwnd);

        if let Ok(mut guard) = shared.back.lock() {
            if guard.is_none() {
                *guard = BackBuffer::new();
            }
            if let Some(back) = guard.as_mut() {
                if back.ensure(cw, ch) {
                    let frame = shared.render.lock().ok();
                    let overlay = if shared.overlay_enabled() {
                        shared.overlay()
                    } else {
                        None
                    };
                    let queue_us = back.render(
                        cw,
                        ch,
                        dpi,
                        pw,
                        shared.smooth_scale.load(Ordering::Relaxed),
                        frame.as_ref().and_then(|g| g.as_ref()),
                        overlay.as_deref(),
                        panel_state,
                    );
                    drop(frame);
                    back.blit_to(hdc, cw, ch);
                    shared.queue_us.store(queue_us, Ordering::Relaxed);
                    if debug_on()
                        && shared.paint_count.load(Ordering::Relaxed) % 40 == 0
                    {
                        println!(
                            "[paint] 视频 {:.1} ms / 面板 {:.1} ms（近 40 帧平均）",
                            back.us_video as f64 / 40.0 / 1000.0,
                            back.us_panel as f64 / 40.0 / 1000.0
                        );
                        back.us_video = 0;
                        back.us_panel = 0;
                    }
                }
            }
        }

        let _ = EndPaint(hwnd, &ps);
    }
    shared
        .paint_us
        .fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
    shared.paint_timed.fetch_add(1, Ordering::Relaxed);
}

/// 把一帧 BGRA 缩放并贴进后备位图的指定矩形。
///
/// 为什么不用 GDI：`StretchDIBits` 的 HALFTONE 是纯软件逐像素卷积，
/// 1200×2670 → 575×1279 实测 17~35 ms（上屏被压到 24fps）；换成 COLORONCOLOR
/// 虽然 1.2ms，但最近邻会把 4:2:0 的色度采样点错位，缩小的画面上只剩彩边。
/// 这里自己做 2×2 盒式平均 + 多线程，1~2ms 就能两者兼得。
fn scale_into(
    dst: *mut u32,
    dst_stride: usize,
    dst_w: usize,
    dst_h: usize,
    rect: (i32, i32, i32, i32),
    f: &FrameSurface,
    smooth: bool,
) {
    let (ox, oy, ow, oh) = rect;
    if dst.is_null() || ow <= 0 || oh <= 0 || f.width == 0 || f.height == 0 {
        return;
    }
    let sw = f.width;
    let sh = f.height;
    if f.pixels.len() < sw * sh {
        return;
    }
    let src = &f.pixels[..sw * sh];

    // 目标矩形不能越过后备位图
    let ox = ox.max(0) as usize;
    let oy = oy.max(0) as usize;
    let ow = (ow as usize).min(dst_w.saturating_sub(ox));
    let oh = (oh as usize).min(dst_h.saturating_sub(oy));
    if ow == 0 || oh == 0 {
        return;
    }

    // 每个目标列对应的源列区间先算好，省掉逐像素除法
    let mut xs: Vec<(u32, u32)> = Vec::with_capacity(ow);
    for dx in 0..ow {
        let sx0 = dx * sw / ow;
        let sx1 = ((dx + 1) * sw / ow).max(sx0 + 1).min(sw);
        xs.push((sx0 as u32, sx1 as u32));
    }

    let threads = if ow * oh < 150_000 {
        1
    } else {
        // 每帧都要建线程，数量别太多：4 段已经足够摊平 735k 像素的活，
        // 再多只是把创建/唤醒的开销翻倍
        std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(4)
            .max(1)
    };
    let rows_per = oh.div_ceil(threads).max(1);
    let addr = dst as usize;

    std::thread::scope(|scope| {
        for band in 0..threads {
            let y0 = band * rows_per;
            if y0 >= oh {
                break;
            }
            let y1 = ((band + 1) * rows_per).min(oh);
            let xs = &xs;
            scope.spawn(move || {
                let dst = addr as *mut u32;
                for dy in y0..y1 {
                    let sy0 = dy * sh / oh;
                    let sy1 = ((dy + 1) * sh / oh).max(sy0 + 1).min(sh);
                    let row = unsafe { dst.add((oy + dy) * dst_stride + ox) };
                    for dx in 0..ow {
                        let (sx0, sx1) = (xs[dx].0 as usize, xs[dx].1 as usize);
                        let px = if !smooth || (sx1 - sx0 == 1 && sy1 - sy0 == 1) {
                            src[sy0 * sw + sx0]
                        } else {
                            box_avg(src, sw, sx0, sx1, sy0, sy1)
                        };
                        unsafe {
                            *row.add(dx) = px;
                        }
                    }
                }
            });
        }
    });
}

/// 源图像一块矩形的平均色（BGRA）。
///
/// 只取该块的**四角**：2×2 的块等于把四个像素全采到，更大的块也不必逐点遍历，
/// 成本固定。不能用「固定步进」采样——遇到棋盘这类周期图案会整块都落在同一相位上，
/// 缩出来偏成一整片纯色（正是彩边/色斑的来源）。
#[inline]
fn box_avg(src: &[u32], sw: usize, sx0: usize, sx1: usize, sy0: usize, sy1: usize) -> u32 {
    let x0 = sx0;
    let x1 = (sx1 - 1).max(sx0);
    let y0 = sy0;
    let y1 = (sy1 - 1).max(sy0);
    let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
    for (i, y) in [y0, y1].into_iter().enumerate() {
        if i == 1 && y1 == y0 {
            break;
        }
        let row = &src[y * sw..y * sw + sw];
        for (j, x) in [x0, x1].into_iter().enumerate() {
            if j == 1 && x1 == x0 {
                break;
            }
            let p = row[x];
            r += (p >> 16) & 0xFF;
            g += (p >> 8) & 0xFF;
            b += p & 0xFF;
            n += 1;
        }
    }
    if n == 0 {
        return 0xFF00_0000;
    }
    0xFF00_0000 | ((r / n) << 16) | ((g / n) << 8) | (b / n)
}

/// 按层级深度循环取色，让父子关系的框线一眼能分开。
/// 注意存的是 RGB，构造 COLORREF 时要交换 R/B（COLORREF 是 0x00BBGGRR）。
const DEPTH_COLORS: [(u8, u8, u8); 8] = [
    (255, 90, 90),  // 红
    (255, 159, 67), // 橙
    (247, 226, 74), // 黄
    (99, 224, 107), // 绿
    (64, 216, 255), // 青
    (110, 140, 255),// 蓝
    (199, 125, 255),// 紫
    (255, 105, 196),// 粉
];

#[inline]
fn depth_color(depth: u16) -> COLORREF {
    let (r, g, b) = DEPTH_COLORS[(depth as usize) % DEPTH_COLORS.len()];
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

/// 排障开关。每帧查一次环境变量太浪费（paint 一秒上百次），这里缓存一次
pub(crate) fn debug_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SCRCPY_RS_DEBUG").is_some())
}

/// 非 ASCII 文本的攒批定时器 id
const TEXT_TIMER_ID: usize = 0x51;
/// 攒到这么多码元就先发一次，避免长句被拆成很多次粘贴
const TEXT_FLUSH_CHARS: usize = 24;

/// 当前修饰键位（UHID 键盘报文用）
fn mods_byte() -> u8 {
    use crate::hid::keyboard::*;
    unsafe {
        let mut m = 0u8;
        if GetKeyState(VK_CONTROL.0 as i32) < 0 {
            m |= MOD_LEFT_CTRL;
        }
        if GetKeyState(VK_SHIFT.0 as i32) < 0 {
            m |= MOD_LEFT_SHIFT;
        }
        if GetKeyState(VK_MENU.0 as i32) < 0 {
            m |= MOD_LEFT_ALT;
        }
        m
    }
}

/// 相对鼠标：更新按键掩码并发给设备
fn send_mouse_button(shared: &Shared, bit: u8, down: bool) {
    let mut mask = shared.mouse_mask.load(Ordering::Relaxed);
    if down {
        mask |= bit;
    } else {
        mask &= !bit;
    }
    shared.mouse_mask.store(mask, Ordering::Relaxed);
    shared.with_input(|s| s.on_mouse_button(mask));
}

/// 相对鼠标：捕获光标并隐藏，释放时恢复
fn set_mouse_capture(hwnd: HWND, shared: &Shared, on: bool) {
    unsafe {
        if on {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let _ = SetCapture(hwnd);
            // SetCursorPos 要的是屏幕坐标，客户区中心必须先换算
            let mut pt = windows::Win32::Foundation::POINT {
                x: rc.right / 2,
                y: rc.bottom / 2,
            };
            let _ = ClientToScreen(hwnd, &mut pt);
            let _ = SetCursorPos(pt.x, pt.y);
            if !shared.hidden_cursor.swap(true, Ordering::Relaxed) {
                ShowCursor(false);
            }
            shared.dragging.store(false, Ordering::Relaxed);
        } else {
            let _ = ReleaseCapture();
            if shared.hidden_cursor.swap(false, Ordering::Relaxed) {
                ShowCursor(true);
            }
        }
    }
}

/// 点击落在面板上就处理掉，返回 Some(结果) 表示已在面板内部消化
fn try_panel_click(hwnd: HWND, shared: &Shared, mx: i32, my: i32) -> Option<LRESULT> {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let cw = rc.right - rc.left;
    let pw = shared.panel_width(cw);
    if pw == 0 || mx < panel::origin_x(cw, pw) - panel::HANDLE_W {
        return None;
    }
    // 分隔条：按下即进入拖动调宽
    if panel::on_handle(cw, pw, mx) {
        shared.resizing_panel.store(true, Ordering::Relaxed);
        shared.set_handle_hot(true);
        unsafe {
            SetCapture(hwnd);
        }
        return Some(LRESULT(0));
    }
    let st = shared.panel_state();
    let item = panel::hit(cw, pw, mx, my, &st)?;
    match item.action() {
        panel::Action::Overlay => {
            let on = !shared.overlay_enabled();
            shared.set_overlay_enabled(on);
            println!("[ui] 界面层级叠加：{}", if on { "开" } else { "关" });
            shared.request_repaint();
        }
        panel::Action::FpsDown | panel::Action::FpsUp => {
            let up = matches!(item.action(), panel::Action::FpsUp);
            let next = panel::step_fps(shared.render_fps(), up);
            shared.set_render_fps(next);
            println!(
                "[ui] 画面刷新上限：{}",
                if next == 0 {
                    "不限".to_string()
                } else {
                    format!("{next} fps")
                }
            );
            shared.request_repaint();
        }
        // 采集参数只是改「待应用」值，真正生效要重连
        panel::Action::MaxSizeDown | panel::Action::MaxSizeUp => {
            let up = matches!(item.action(), panel::Action::MaxSizeUp);
            let cur = shared.pending_size.load(Ordering::Relaxed).max(0) as u32;
            let next = panel::step_value(&panel::SIZE_STEPS, cur, up);
            shared.pending_size.store(next as i32, Ordering::Relaxed);
            shared.request_repaint();
        }
        panel::Action::BitrateDown | panel::Action::BitrateUp => {
            let up = matches!(item.action(), panel::Action::BitrateUp);
            let cur = shared.pending_bitrate.load(Ordering::Relaxed).max(0) as u32;
            let next = panel::step_value(&panel::BITRATE_STEPS, cur, up);
            shared.pending_bitrate.store(next as i32, Ordering::Relaxed);
            shared.request_repaint();
        }
        panel::Action::MaxFpsDown | panel::Action::MaxFpsUp => {
            let up = matches!(item.action(), panel::Action::MaxFpsUp);
            let cur = shared.pending_fps.load(Ordering::Relaxed).max(0) as u32;
            let next = panel::step_value(&panel::CAPFPS_STEPS, cur, up);
            shared.pending_fps.store(next as i32, Ordering::Relaxed);
            shared.request_repaint();
        }
        // 其余交给上层（app.rs 的 InputSink）执行
        panel::Action::Screenshot => shared.with_input(|s| s.on_command(Command::Screenshot)),
        panel::Action::Record => shared.with_input(|s| s.on_command(Command::ToggleRecord)),
        panel::Action::OpenFolder => shared.with_input(|s| s.on_command(Command::OpenFolder)),
        panel::Action::Clipboard => shared.with_input(|s| s.on_command(Command::ToggleClipboard)),
        panel::Action::Paste => shared.with_input(|s| s.on_command(Command::PasteToDevice)),
        panel::Action::Copy => shared.with_input(|s| s.on_command(Command::FetchClipboard(1))),
        panel::Action::Audio => shared.with_input(|s| s.on_command(Command::ToggleAudio)),
        panel::Action::Smooth => {
            let on = !shared.smooth_scale.load(Ordering::Relaxed);
            shared.smooth_scale.store(on, Ordering::Relaxed);
            println!("[ui] 平滑缩放：{}", if on { "开（清晰，消耗 CPU）" } else { "关（更顺）" });
            shared.request_repaint();
        }
        panel::Action::UhidKeyboard | panel::Action::UhidMouse => {
            let kb = matches!(item.action(), panel::Action::UhidKeyboard);
            if kb {
                let on = !shared.uhid_keyboard.load(Ordering::Relaxed);
                shared.uhid_keyboard.store(on, Ordering::Relaxed);
                println!("[uhid] 键盘：{}", if on { "开" } else { "关" });
                shared.set_notice(if on {
                    "UHID 键盘已开启（组合键在设备内生效）"
                } else {
                    "UHID 键盘已关闭"
                });
            } else {
                let on = !shared.uhid_mouse.load(Ordering::Relaxed);
                shared.uhid_mouse.store(on, Ordering::Relaxed);
                set_mouse_capture(hwnd, shared, on);
                println!("[uhid] 相对鼠标：{}", if on { "开" } else { "关" });
                shared.set_notice(if on {
                    "相对鼠标已开启（Esc 释放）"
                } else {
                    "相对鼠标已关闭"
                });
            }
            shared.request_repaint();
        }
        panel::Action::Reconnect => shared.with_input(|s| s.on_command(Command::Reconnect)),
    }
    Some(LRESULT(0))
}

/// 鼠标在面板上移动时更新高亮；只有变化才触发重画
fn update_panel_hover(hwnd: HWND, shared: &Shared, mx: i32, my: i32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let cw = rc.right - rc.left;
    let pw = shared.panel_width(cw);
    let on_handle = panel::on_handle(cw, pw, mx);
    let next = if on_handle {
        None
    } else {
        panel::hit(cw, pw, mx, my, &shared.panel_state())
    };
    if next != shared.hover() || on_handle != shared.handle_hot() {
        shared.set_hover(next);
        shared.set_handle_hot(on_handle);
        shared.request_repaint();
    }
}

/// 设备像素矩形 -> 客户区像素矩形（叠加层与画面的对齐全靠这一步）
fn map_device_rect(
    rect: crate::uitree::Rect,
    ox: i32,
    oy: i32,
    sx: f64,
    sy: f64,
) -> (i32, i32, i32, i32) {
    (
        ox + (rect.l as f64 * sx).round() as i32,
        oy + (rect.t as f64 * sy).round() as i32,
        ox + (rect.r as f64 * sx).round() as i32,
        oy + (rect.b as f64 * sy).round() as i32,
    )
}

/// 在已缩放的视频区域内绘制界面层级框线
fn draw_overlay(hdc: HDC, ox: i32, oy: i32, ow: i32, oh: i32, snap: &UiSnapshot) {
    if ow <= 0 || oh <= 0 || snap.is_empty() {
        return;
    }
    let (sw, sh) = snap.screen;
    if sw <= 0 || sh <= 0 {
        return;
    }
    // 设备像素 -> 客户区像素（-m 缩放、旋转后都能自动对齐）
    let sx = ow as f64 / sw as f64;
    let sy = oh as f64 / sh as f64;

    unsafe {
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let old_font = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT);

        for n in &snap.nodes {
            // 设备像素下就退化的节点直接跳过，省掉后面的换算
            if n.rect.width() < 2 || n.rect.height() < 2 {
                continue;
            }
            let (l, t, r, b) = map_device_rect(n.rect, ox, oy, sx, sy);
            // 退化成一两个像素的容器画出来只是一团噪点，跳过
            if r - l < 3 || b - t < 3 {
                continue;
            }
            let color = depth_color(n.depth);
            let brush = CreateSolidBrush(color);
            let rc = RECT {
                left: l,
                top: t,
                right: r,
                bottom: b,
            };
            FrameRect(hdc, &rc, brush);
            let _ = DeleteObject(brush);

            // 空间够才画标签，否则整屏都是字
            if r - l >= 56 && b - t >= 18 {
                let label = label_for(n);
                SetTextColor(hdc, color);
                let w = wide(&label);
                let _ = TextOutW(hdc, l + 2, t + 1, &w[..w.len() - 1]);
            }
        }

        SelectObject(hdc, old_font);
    }
}

/// 标签内容：`d2 TextView · 设置`（深度 + 类名 + 文本/资源 id）
fn label_for(n: &crate::uitree::UiNode) -> String {
    let mut s = format!("d{} {}", n.depth, n.class);
    if !n.text.is_empty() {
        s.push_str(" · ");
        s.push_str(&truncate(&n.text, 24));
    } else if !n.res_id.is_empty() {
        // resource-id 只留 `:id/` 之后的部分
        let tail = n.res_id.rsplit("/id/").next().unwrap_or(&n.res_id);
        s.push_str(" · #");
        s.push_str(&truncate(tail, 24));
    }
    if n.clickable {
        s.push('·');
        s.push('●');
    }
    if n.scrollable {
        s.push_str(" ↕");
    }
    s
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// 把客户区坐标换算成视频坐标；`clamp` 为 true 时落在黑边上也夹到画面内
/// （拖动过程中手指移出画面不能丢事件，否则设备端收不到 ACTION_UP，触摸会「卡住」）
fn client_to_video_clamped(
    hwnd: HWND,
    shared: &Shared,
    mx: i32,
    my: i32,
    clamp: bool,
) -> Option<(i32, i32, u32, u32)> {
    let (vw, vh) = shared.video_size();
    if vw == 0 || vh == 0 {
        return None;
    }
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let cw = rc.right - rc.left;
    let (x, y, w, h) = video_rect(cw, rc.bottom - rc.top, shared.panel_width(cw), vw as i32, vh as i32);
    if w <= 0 || h <= 0 {
        return None;
    }
    if !clamp && (mx < x || my < y || mx >= x + w || my >= y + h) {
        return None;
    }
    let rel_x = (mx - x).clamp(0, w - 1) as f64;
    let rel_y = (my - y).clamp(0, h - 1) as f64;
    let vx = (rel_x / w as f64 * vw as f64).round() as i32;
    let vy = (rel_y / h as f64 * vh as f64).round() as i32;
    Some((vx.clamp(0, vw as i32 - 1), vy.clamp(0, vh as i32 - 1), vw, vh))
}

fn client_to_video(hwnd: HWND, shared: &Shared, mx: i32, my: i32) -> Option<(i32, i32, u32, u32)> {
    client_to_video_clamped(hwnd, shared, mx, my, false)
}

pub(crate) fn meta_state() -> u32 {
    unsafe {
        let mut meta = 0u32;
        if GetKeyState(VK_SHIFT.0 as i32) < 0 {
            meta |= crate::control::META_SHIFT_ON;
        }
        if GetKeyState(VK_CONTROL.0 as i32) < 0 {
            meta |= crate::control::META_CTRL_ON;
        }
        if GetKeyState(VK_MENU.0 as i32) < 0 {
            meta |= crate::control::META_ALT_ON;
        }
        meta
    }
}

/// Windows VK -> Android keycode；None 表示不处理
pub(crate) fn vk_to_keycode(vk: u32) -> Option<u32> {
    use crate::control::*;
    let kc = match vk {
        0x30..=0x39 => KEYCODE_0 + (vk - 0x30),
        0x41..=0x5A => KEYCODE_A + (vk - 0x41),
        0x70..=0x7B => KEYCODE_F1 + (vk - 0x70),
        0x08 => KEYCODE_DEL,
        0x09 => KEYCODE_TAB,
        0x0D => KEYCODE_ENTER,
        0x1B => KEYCODE_BACK,
        0x20 => KEYCODE_SPACE,
        0x21 => KEYCODE_PAGE_UP,
        0x22 => KEYCODE_PAGE_DOWN,
        0x23 => KEYCODE_MOVE_END,
        0x24 => KEYCODE_MOVE_HOME,
        0x25 => KEYCODE_DPAD_LEFT,
        0x26 => KEYCODE_DPAD_UP,
        0x27 => KEYCODE_DPAD_RIGHT,
        0x28 => KEYCODE_DPAD_DOWN,
        0x2E => KEYCODE_FORWARD_DEL,
        0x5B => KEYCODE_APP_SWITCH,
        0xAD => KEYCODE_VOLUME_MUTE,
        0xAE => KEYCODE_VOLUME_DOWN,
        0xAF => KEYCODE_VOLUME_UP,
        0xBA => KEYCODE_SEMICOLON,
        0xBB => KEYCODE_EQUALS,
        0xBC => KEYCODE_COMMA,
        0xBD => KEYCODE_MINUS,
        0xBE => KEYCODE_PERIOD,
        0xBF => KEYCODE_SLASH,
        0xC0 => KEYCODE_GRAVE,
        0xDB => KEYCODE_LEFT_BRACKET,
        0xDC => KEYCODE_BACKSLASH,
        0xDD => KEYCODE_RIGHT_BRACKET,
        0xDE => KEYCODE_APOSTROPHE,
        _ => return None,
    };
    Some(kc)
}

fn handle_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, shared: &Shared) -> LRESULT {
    // 鼠标坐标（客户区）
    let mx = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
    let my = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;

    match msg {
        WM_PAINT => {
            paint(hwnd, shared);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SETCURSOR => {
            let mut rc = RECT::default();
            unsafe {
                let _ = GetClientRect(hwnd, &mut rc);
            }
            let cw = rc.right - rc.left;
            if panel::on_handle(cw, shared.panel_width(cw), mx)
                || shared.resizing_panel.load(Ordering::Relaxed)
            {
                unsafe {
                    if let Ok(cur) = LoadCursorW(None, IDC_SIZEWE) {
                        SetCursor(cur);
                    }
                }
                return LRESULT(1);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_SIZE => {
            unsafe {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if shared.uhid_mouse.load(Ordering::Relaxed) {
                send_mouse_button(shared, crate::hid::mouse::BTN_LEFT, true);
                return LRESULT(0);
            }
            // 点面板不当作触摸
            if let Some(handled) = try_panel_click(hwnd, shared, mx, my) {
                return handled;
            }
            if let Some((vx, vy, vw, vh)) = client_to_video(hwnd, shared, mx, my) {
                shared.dragging.store(true, Ordering::Relaxed);
                // 捕获鼠标：拖到窗口外也能继续收到 MOVE/UP
                unsafe {
                    SetCapture(hwnd);
                }
                shared
                    .with_input(|s| s.on_touch(crate::control::ACTION_DOWN, vx, vy, vw, vh, true));
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // 相对鼠标：只发位移增量，然后把光标钉回窗口中心，避免撞边
            if shared.uhid_mouse.load(Ordering::Relaxed)
                && !shared.resizing_panel.load(Ordering::Relaxed)
            {
                let mut rc = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut rc);
                }
                let (cx, cy) = (rc.right / 2, rc.bottom / 2);
                let dx = mx - cx;
                let dy = my - cy;
                if dx != 0 || dy != 0 {
                    shared.with_input(|s| s.on_mouse_move(dx, dy));
                    unsafe {
                        let mut pt = windows::Win32::Foundation::POINT { x: cx, y: cy };
                        let _ = ClientToScreen(hwnd, &mut pt);
                        let _ = SetCursorPos(pt.x, pt.y);
                    }
                }
                return LRESULT(0);
            }
            if shared.resizing_panel.load(Ordering::Relaxed) {
                let mut rc = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut rc);
                }
                let cw = rc.right - rc.left;
                // 面板宽度 = 客户区右边缘 - 鼠标 x
                shared.set_panel_width(cw - mx);
                shared.request_repaint();
                return LRESULT(0);
            }
            if !shared.dragging.load(Ordering::Relaxed) {
                // 申请 WM_MOUSELEAVE，鼠标移出窗口时才能把 hover 清掉
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe {
                    let _ = TrackMouseEvent(&mut tme);
                }
                update_panel_hover(hwnd, shared, mx, my);
            }
            if shared.dragging.load(Ordering::Relaxed) {
                if let Some((vx, vy, vw, vh)) = client_to_video_clamped(hwnd, shared, mx, my, true)
                {
                    shared.with_input(|s| {
                        s.on_touch(crate::control::ACTION_MOVE, vx, vy, vw, vh, true)
                    });
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if shared.uhid_mouse.load(Ordering::Relaxed) {
                send_mouse_button(shared, crate::hid::mouse::BTN_LEFT, false);
                return LRESULT(0);
            }
            if shared.resizing_panel.swap(false, Ordering::Relaxed) {
                unsafe {
                    let _ = ReleaseCapture();
                }
                return LRESULT(0);
            }
            // 只要按下过就一定要发 ACTION_UP，哪怕松手时已经在画面外
            if shared.dragging.swap(false, Ordering::Relaxed) {
                unsafe {
                    let _ = ReleaseCapture();
                }
                if let Some((vx, vy, vw, vh)) =
                    client_to_video_clamped(hwnd, shared, mx, my, true)
                {
                    shared
                        .with_input(|s| s.on_touch(crate::control::ACTION_UP, vx, vy, vw, vh, false));
                }
            }
            LRESULT(0)
        }
        // WM_DROPFILES
        0x0233 => {
            let hdrop = HDROP(wparam.0 as *mut core::ffi::c_void);
            let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
            let mut paths = Vec::new();
            for i in 0..count {
                let len = unsafe { DragQueryFileW(hdrop, i, None) } as usize;
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; len + 1];
                unsafe {
                    DragQueryFileW(hdrop, i, Some(&mut buf));
                }
                paths.push(String::from_utf16_lossy(&buf[..len]));
            }
            unsafe { DragFinish(hdrop) };
            if !paths.is_empty() {
                shared.with_input(|s| s.on_files(&paths));
            }
            LRESULT(0)
        }
        // WM_MOUSELEAVE（0x02A3，windows crate 没有导出这个常量）
        0x02A3 => {
            if shared.hover().is_some() {
                shared.set_hover(None);
                shared.request_repaint();
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            if shared.uhid_mouse.load(Ordering::Relaxed) {
                send_mouse_button(shared, crate::hid::mouse::BTN_RIGHT, true);
                return LRESULT(0);
            }
            shared.with_input(|s| s.on_command(Command::Back));
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            if shared.uhid_mouse.load(Ordering::Relaxed) {
                send_mouse_button(shared, crate::hid::mouse::BTN_MIDDLE, true);
                return LRESULT(0);
            }
            shared.with_input(|s| s.on_command(Command::Home));
            LRESULT(0)
        }
        WM_XBUTTONDOWN => {
            let btn = ((wparam.0 >> 16) & 0xFFFF) as u32;
            shared.with_input(|s| {
                s.on_command(if btn == 1 {
                    Command::Back
                } else {
                    Command::Home
                })
            });
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            if shared.uhid_mouse.load(Ordering::Relaxed) {
                let notches = delta / 120;
                shared.with_input(|s| s.on_mouse_wheel(notches));
                return LRESULT(0);
            }
            let mut pt = windows::Win32::Foundation::POINT { x: mx, y: my };
            unsafe {
                let _ = ScreenToClient(hwnd, &mut pt);
            }
            if let Some((vx, vy, vw, vh)) = client_to_video(hwnd, shared, pt.x, pt.y) {
                let v = delta as f32 / 120.0;
                shared.with_input(|s| s.on_scroll(vx, vy, vw, vh, v));
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wparam.0 as u32;
            // Esc 优先用来退出相对鼠标（否则用户会被困在捕获里）
            if vk == 0x1B && shared.uhid_mouse.load(Ordering::Relaxed) {
                shared.uhid_mouse.store(false, Ordering::Relaxed);
                set_mouse_capture(hwnd, shared, false);
                shared.set_notice("相对鼠标已关闭");
                shared.request_repaint();
                return LRESULT(0);
            }
            if vk == 0x7A {
                // F11
                toggle_fullscreen_raw(hwnd, shared);
                return LRESULT(0);
            }
            let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 };
            let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 };
            if ctrl {
                // 音量/多任务是纯按键注入，直接走 on_key，不用单独的命令
                let key = match vk {
                    0x26 => Some(crate::control::KEYCODE_VOLUME_UP),
                    0x28 => Some(crate::control::KEYCODE_VOLUME_DOWN),
                    0x4D => Some(crate::control::KEYCODE_VOLUME_MUTE),
                    0x53 => Some(crate::control::KEYCODE_APP_SWITCH),
                    _ => None,
                };
                if let Some(kc) = key {
                    shared.with_input(|s| {
                        s.on_key(crate::control::KEY_ACTION_DOWN, kc, 0);
                        s.on_key(crate::control::KEY_ACTION_UP, kc, 0);
                    });
                    return LRESULT(0);
                }

                let cmd = match (vk, shift) {
                    (0x52, false) => Some(Command::Rotate),
                    (0x52, true) => Some(Command::ResetVideo),
                    (0x48, _) => Some(Command::Home),
                    (0x42, _) => Some(Command::Back),
                    (0x50, _) => Some(Command::Power),
                    (0x4F, _) => Some(Command::ToggleScreen),
                    (0x55, _) => Some(Command::ToggleOverlay),
                    (0x56, _) => Some(Command::PasteToDevice),
                    (0x43, false) => Some(Command::FetchClipboard(1)),
                    (0x43, true) => Some(Command::CollapsePanels),
                    (0x58, _) => Some(Command::FetchClipboard(2)),
                    (0x4E, false) => Some(Command::ExpandNotification),
                    (0x4E, true) => Some(Command::ExpandSettings),
                    (0x47, _) => Some(Command::StartApp),
                    (0x51, _) => Some(Command::Quit),
                    _ => None,
                };
                if let Some(c) = cmd {
                    shared.with_input(|s| s.on_command(c));
                    return LRESULT(0);
                }
            }
            if shared.uhid_keyboard.load(Ordering::Relaxed) {
                let mods = mods_byte();
                shared.with_input(|s| s.on_key_raw(vk, true, mods));
                return LRESULT(0);
            }
            if let Some(kc) = vk_to_keycode(vk) {
                let meta = meta_state();
                shared.with_input(|s| s.on_key(crate::control::KEY_ACTION_DOWN, kc, meta));
            }
            LRESULT(0)
        }
        WM_KEYUP | WM_SYSKEYUP => {
            let vk = wparam.0 as u32;
            if shared.uhid_keyboard.load(Ordering::Relaxed) {
                let mods = mods_byte();
                shared.with_input(|s| s.on_key_raw(vk, false, mods));
                return LRESULT(0);
            }
            if let Some(kc) = vk_to_keycode(vk) {
                let meta = meta_state();
                shared.with_input(|s| s.on_key(crate::control::KEY_ACTION_UP, kc, meta));
            }
            LRESULT(0)
        }
        WM_CHAR => {
            let c = wparam.0 as u16;
            // 0xFFFF（菜单）/ 0xFFFE 这类伪字符直接丢
            if c == 0xFFFF || c == 0xFFFE {
                return LRESULT(0);
            }
            // ASCII 可见字符走 keycode 通道（WM_KEYDOWN 已处理），这里只收非 ASCII
            if c > 0x7F {
                let mut full = false;
                if let Ok(mut g) = shared.text_buf.lock() {
                    // 存码元不存 char：emoji 等代理对不会被拆成两个替换字符
                    g.push(c);
                    full = g.len() >= TEXT_FLUSH_CHARS;
                }
                if full {
                    // 一次粘贴十几二十个字就够快了，别为等「安静」把长句拖成多次粘贴
                    unsafe {
                        let _ = KillTimer(hwnd, TEXT_TIMER_ID);
                    }
                    let text = shared.take_text();
                    if !text.is_empty() {
                        shared.with_input(|s| s.on_text(&text));
                    }
                } else {
                    // 60ms 内没有新字符就整段送出。输入法一次提交的多个 WM_CHAR 是连着
                    // 投递的（间隔≈0），所以这个窗口只用来「等这一批到齐」，不宜太长——
                    // 每多等一毫秒，用户就晚一点看到字上去
                    unsafe {
                        SetTimer(hwnd, TEXT_TIMER_ID, 60, None);
                    }
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TEXT_TIMER_ID => {
            unsafe {
                let _ = KillTimer(hwnd, TEXT_TIMER_ID);
            }
            let text = shared.take_text();
            if !text.is_empty() {
                shared.with_input(|s| s.on_text(&text));
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // 关窗前把还没送出的中文补发出去
            let text = shared.take_text();
            if !text.is_empty() {
                shared.with_input(|s| s.on_text(&text));
            }
            shared.quit.store(true, Ordering::Relaxed);
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn toggle_fullscreen_raw(hwnd: HWND, shared: &Shared) {
    unsafe {
        let next = !shared.fullscreen.load(Ordering::Relaxed);
        shared.fullscreen.store(next, Ordering::Relaxed);
        if next {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            SetWindowLongPtrW(hwnd, GWL_STYLE, (style & !WS_OVERLAPPEDWINDOW.0) as isize);
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut mi).as_bool() {
                let r = mi.rcMonitor;
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_SHOWWINDOW,
                );
            }
        } else {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            SetWindowLongPtrW(hwnd, GWL_STYLE, (style | WS_OVERLAPPEDWINDOW.0) as isize);
            let _ = SetWindowPos(
                hwnd,
                HWND_NOTOPMOST,
                100,
                100,
                900,
                600,
                SWP_SHOWWINDOW,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uitree::Rect;

    fn frame(pixels: Vec<u32>, width: usize, height: usize) -> FrameSurface {
        FrameSurface {
            pixels,
            width,
            height,
            arrived: std::time::Instant::now(),
        }
    }

    /// 1:1 时缩放必须逐像素原样拷贝（否则叠加层/截图会与画面错位）
    #[test]
    fn scale_identity_is_lossless() {
        let (w, h) = (4usize, 4usize);
        let mut px = vec![0u32; w * h];
        for (i, p) in px.iter_mut().enumerate() {
            *p = 0xFF00_0000 | (i as u32 * 0x01_0305);
        }
        let f = frame(px.clone(), w, h);
        let mut dst = vec![0u32; w * h];
        scale_into(dst.as_mut_ptr(), w, w, h, (0, 0, w as i32, h as i32), &f, true);
        assert_eq!(dst, px);
    }

    /// 2:1 缩小：左半红、右半蓝，盒式平均后仍然是纯红/纯蓝，不能出现「彩边」
    #[test]
    fn scale_box_keeps_flat_colors() {
        let (w, h) = (4usize, 4usize);
        let red = 0xFFFF_0000u32;
        let blue = 0xFF00_00FFu32;
        let mut px = vec![0u32; w * h];
        for y in 0..h {
            for x in 0..w {
                px[y * w + x] = if x < 2 { red } else { blue };
            }
        }
        let f = frame(px, w, h);
        let (dw, dh) = (2usize, 2usize);
        let mut dst = vec![0u32; dw * dh];
        scale_into(dst.as_mut_ptr(), dw, dw, dh, (0, 0, 2, 2), &f, true);
        // 左列全是红，右列全是蓝
        assert_eq!(dst[0], red);
        assert_eq!(dst[1], blue);
        assert_eq!(dst[2], red);
        assert_eq!(dst[3], blue);
    }

    /// 黑白棋盘缩到 1 个像素，应该是中间灰而不是纯黑/纯白（最近邻会失真）
    #[test]
    fn scale_averages_checkerboard() {
        let (w, h) = (4usize, 4usize);
        let mut px = vec![0u32; w * h];
        for y in 0..h {
            for x in 0..w {
                px[y * w + x] = if (x + y) % 2 == 0 {
                    0xFFFF_FFFF
                } else {
                    0xFF00_0000
                };
            }
        }
        let f = frame(px, w, h);
        let mut dst = vec![0u32; 1];
        scale_into(dst.as_mut_ptr(), 1, 1, 1, (0, 0, 1, 1), &f, true);
        let r = (dst[0] >> 16) & 0xFF;
        assert!(r > 100 && r < 160, "棋盘应被平均成灰, 实际 {r}");
    }

    /// 目标矩形被面板/窗口挤出边界时不能越界写
    #[test]
    fn scale_clips_to_backbuffer() {
        let f = frame(vec![0xFFFF_FFFFu32; 4], 2, 2);
        let mut dst = vec![0u32; 4];
        // 只允许写 (0,0)-(2,2) 里的右下一象限
        scale_into(dst.as_mut_ptr(), 2, 2, 2, (1, 1, 4, 4), &f, false);
        assert_eq!(dst[0], 0, "不能写到左上是越界");
        assert_eq!(dst[3], 0xFFFF_FFFF, "右下应该在范围内");
    }

    /// 叠加层的坐标必须用「视频矩形」而不是「客户区矩形」，
    /// 否则有黑边或窗口比例不一致时框线就会整体偏移。
    #[test]
    fn overlay_maps_screen_onto_video_rect() {
        // 客户区 383x852，视频 1200x2670 -> 铺满客户区
        let (x, y, w, h) = fit_rect(383, 852, 1200, 2670);
        assert_eq!((x, y, w, h), (0, 0, 383, 852));
        let sx = w as f64 / 1200.0;
        let sy = h as f64 / 2670.0;

        // 整屏 -> 正好是整个视频矩形
        let full = Rect { l: 0, t: 0, r: 1200, b: 2670 };
        assert_eq!(map_device_rect(full, x, y, sx, sy), (0, 0, 383, 852));

        // 右下四分之一 -> 视频矩形的右下四分之一（600*383/1200=191.5，四舍五入到 192）
        let q = Rect { l: 600, t: 1335, r: 1200, b: 2670 };
        assert_eq!(map_device_rect(q, x, y, sx, sy), (192, 426, 383, 852));
    }

    /// 窗口比视频宽时会出现左右黑边，映射必须以黑边之后的 x 为原点
    #[test]
    fn overlay_respects_letterbox_offset() {
        let (x, y, w, h) = fit_rect(1000, 852, 1200, 2670);
        assert!(x > 0 && y == 0, "预期左右黑边: {x},{y},{w},{h}");
        let sx = w as f64 / 1200.0;
        let sy = h as f64 / 2670.0;
        let corner = Rect { l: 0, t: 0, r: 1200, b: 2670 };
        let (l, t, r, b) = map_device_rect(corner, x, y, sx, sy);
        assert_eq!((l, t), (x, y));
        assert_eq!((r, b), (x + w, y + h));
    }

    /// -m 1024 时视频比屏幕小，叠加层要按视频尺寸缩放
    #[test]
    fn overlay_scales_with_downscaled_video() {
        // 屏幕 1200x2670，视频被压到最长边 1200*? -> 1024 宽时高度 2278
        let snap_screen = (1200, 2670);
        let video = (1024, 2278);
        let (x, y, w, h) = fit_rect(512, 1140, video.0, video.1);
        let sx = w as f64 / snap_screen.0 as f64;
        let sy = h as f64 / snap_screen.1 as f64;
        let full = Rect { l: 0, t: 0, r: 1200, b: 2670 };
        let (l, t, r, b) = map_device_rect(full, x, y, sx, sy);
        assert_eq!((l, t, r, b), (x, y, x + w, y + h));
    }
}
