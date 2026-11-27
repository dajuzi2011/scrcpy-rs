//! 应用编排：把 adb/协议/解码/窗口串起来，并实现输入事件到控制消息的映射。

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bmp;
use crate::control::*;
use crate::error::{Error, Result};
use crate::mf::{self, Decoder};
use crate::sps;
use crate::video::{Packet, VideoReader};
use crate::window::{Command, FrameSurface, InputSink, Shared, Window};
use crate::yuv::{nv12_to_bgra, Matrix};

/// 把输入事件翻译成控制通道消息，同时负责面板上那些「动作类」功能
struct AppController {
    ctrl: Arc<Mutex<Option<Controller>>>,
    shared: Arc<Shared>,
    adb: crate::adb::Adb,
    serial: String,
    audio: Option<Arc<crate::audio::AudioPlayer>>,
    out_dir: std::path::PathBuf,
    /// --start-app 指定的包名
    start_app: Option<String>,
    buttons: u32,
    /// UHID 键盘状态
    uhid_kb_ready: bool,
    uhid_kb: crate::hid::keyboard::Keyboard,
    /// UHID 鼠标状态
    uhid_mouse_ready: bool,
    mouse_buttons: u8,
    /// 自己维护的修饰键位
    uhid_mods: u8,
    /// 上面这些 UHID 状态属于哪一代控制通道
    uhid_epoch: u32,
}

impl AppController {
    fn new(
        ctrl: Arc<Mutex<Option<Controller>>>,
        shared: Arc<Shared>,
        adb: crate::adb::Adb,
        serial: String,
        audio: Option<Arc<crate::audio::AudioPlayer>>,
        out_dir: std::path::PathBuf,
        start_app: Option<String>,
    ) -> Self {
        Self {
            ctrl,
            shared,
            adb,
            serial,
            audio,
            out_dir,
            start_app,
            buttons: 0,
            uhid_kb_ready: false,
            uhid_kb: crate::hid::keyboard::Keyboard::new(),
            uhid_mouse_ready: false,
            mouse_buttons: 0,
            uhid_mods: 0,
            uhid_epoch: 0,
        }
    }

    /// 控制通道换代（重连 / 改参数重连）后，本地缓存的 UHID 状态必须作废：
    /// 虚拟设备是 server 进程里的资源，新进程里并没有 id 1/2 的设备，
    /// 继续按「已创建」发 UHID_INPUT 就会让设备端刷 "Unknown UHID id"。
    fn sync_uhid_epoch(&mut self) {
        let epoch = self.shared.control_epoch();
        if epoch == self.uhid_epoch {
            return;
        }
        if self.uhid_kb_ready || self.uhid_mouse_ready {
            println!("[uhid] 会话已重建，虚拟键鼠需要重新创建");
        }
        self.uhid_epoch = epoch;
        self.uhid_kb_ready = false;
        self.uhid_mouse_ready = false;
        self.uhid_kb = crate::hid::keyboard::Keyboard::new();
        self.uhid_mods = 0;
        self.mouse_buttons = 0;
        self.shared.reset_mouse_mask();
    }

    /// 发送并返回是否成功（UHID 创建需要知道结果）
    fn with_ok<F: FnOnce(&Controller) -> Result<()>>(&self, f: F) -> bool {
        let mut guard = match self.ctrl.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        match guard.as_ref() {
            Some(c) => {
                if f(c).is_ok() {
                    true
                } else {
                    *guard = None;
                    false
                }
            }
            None => false,
        }
    }

    fn ensure_uhid_keyboard(&mut self) -> bool {
        self.sync_uhid_epoch();
        if self.uhid_kb_ready {
            return true;
        }
        let ok = self.with_ok(|c| {
            c.uhid_create(
                crate::hid::KEYBOARD_ID,
                0x1209,
                0x0001,
                "scrcpy-rs Keyboard",
                crate::hid::keyboard::REPORT_DESC,
            )
        });
        if ok {
            self.uhid_kb_ready = true;
            // 先发一帧"全松开"，避免上一代会话残留的按键卡住
            let clear = self.uhid_kb.release_all();
            self.with(|c| c.uhid_input(crate::hid::KEYBOARD_ID, &clear));
            println!("[uhid] 虚拟键盘已创建（组合键在设备内生效）");
        } else {
            eprintln!("[uhid] 虚拟键盘创建失败，按键被丢弃");
        }
        ok
    }

    fn ensure_uhid_mouse(&mut self) -> bool {
        self.sync_uhid_epoch();
        if self.uhid_mouse_ready {
            return true;
        }
        let ok = self.with_ok(|c| {
            c.uhid_create(
                crate::hid::MOUSE_ID,
                0x1209,
                0x0002,
                "scrcpy-rs Mouse",
                crate::hid::mouse::REPORT_DESC,
            )
        });
        if ok {
            self.uhid_mouse_ready = true;
            println!("[uhid] 虚拟鼠标已创建");
        }
        ok
    }

    fn with<F: FnOnce(&Controller) -> Result<()>>(&self, f: F) {
        let mut guard = match self.ctrl.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(c) = guard.as_ref() {
            if let Err(e) = f(c) {
                // 断线后 socket 已废：第一次失败就丢掉句柄，否则每次触摸都会刷一行日志。
                // 重连成功后 run_device 会把新的 Controller 放进 slot，输入自然恢复。
                eprintln!("[control] 控制通道不可用（{e}），等待重连");
                *guard = None;
            }
        }
    }

    /// 截图：把当前上屏的那一帧写成 PNG
    fn screenshot(&self) {
        let frame = match self.shared.render_snapshot() {
            Some(f) => f,
            None => {
                eprintln!("[shot] 当前没有可用的画面（render 槽为空）");
                self.shared.set_notice("还没有画面，截图失败");
                return;
            }
        };
        println!("[shot] 取到 {}x{} 帧", frame.width, frame.height);
        let path = crate::record::default_path(&self.out_dir, "shot", "png");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let enc = match crate::screenshot::WicEncoder::new() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[shot] WIC 初始化失败: {e}");
                self.shared.set_notice(format!("截图失败: {e}"));
                return;
            }
        };
        match enc.write_png(&path, &frame.pixels, frame.width, frame.height) {
            Ok(()) => {
                println!("[shot] 已保存 {}", path.display());
                self.shared
                    .set_notice(format!("截图已保存: {}", path.display()));
            }
            Err(e) => {
                eprintln!("[shot] 保存失败: {e}");
                self.shared.set_notice(format!("截图失败: {e}"));
            }
        }
    }

    fn open_folder(&self) {
        let dir = self.out_dir.clone();
        let _ = std::fs::create_dir_all(&dir);
        let wide = |s: &str| -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        };
        let op = wide("open");
        let path = wide(&dir.to_string_lossy());
        unsafe {
            windows::Win32::UI::Shell::ShellExecuteW(
                None,
                windows::core::PCWSTR(op.as_ptr()),
                windows::core::PCWSTR(path.as_ptr()),
                None,
                None,
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
            );
        }
    }

    /// 把拖进来的文件推到设备的下载目录（走 adb push，与官方 scrcpy 一致）
    fn push_files(&self, paths: &[String]) {
        let adb = self.adb.clone();
        let serial = self.serial.clone();
        let shared = self.shared.clone();
        let ctrl_for_scan = self.ctrl.clone();
        let paths: Vec<String> = paths.to_vec();
        std::thread::spawn(move || {
            for p in paths {
                let name = std::path::Path::new(&p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".into());
                shared.set_notice(format!("推送中: {name}"));
                let remote = format!("/sdcard/Download/{name}");
                match adb.push(&serial, std::path::Path::new(&p), &remote) {
                    Ok(()) => {
                        println!("[push] {name} -> {remote}");
                        shared.set_notice(format!("已推送: {name}"));
                        // 让设备的媒体库刷新，否则相册/文件管理里看不到
                        if let Ok(g) = ctrl_for_scan.lock() {
                            if let Some(c) = g.as_ref() {
                                let _ = c.scan_file(&remote);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[push] {name} 失败: {e}");
                        shared.set_notice(format!("推送失败: {name}"));
                    }
                }
            }
        });
    }
}

impl InputSink for AppController {
    fn on_touch(&mut self, action: u8, x: i32, y: i32, vw: u32, vh: u32, _down: bool) {
        // 按下/抬起打一行日志：排查「点了没反应」时最有用的一条线索
        if action != ACTION_MOVE {
            let name = if action == ACTION_DOWN { "按下" } else { "抬起" };
            println!("[touch] {name} ({x}, {y}) 画面 {vw}x{vh}");
        }
        // ACTION_MOVE 的 actionButton 必须是 0，按下/抬起才带主键
        let (action_button, buttons) = match action {
            ACTION_DOWN => (BUTTON_PRIMARY, BUTTON_PRIMARY),
            ACTION_UP => (BUTTON_PRIMARY, 0),
            _ => (0, BUTTON_PRIMARY),
        };
        self.buttons = buttons;
        self.with(|c| {
            c.touch(
                action,
                // 与官方客户端一致：走「通用手指」这个 well-known pointer id，
                // 设备端会把它当作真实手指（TOOL_TYPE_FINGER / SOURCE_TOUCHSCREEN）
                POINTER_ID_GENERIC_FINGER,
                x,
                y,
                vw as u16,
                vh as u16,
                1.0,
                action_button,
                buttons,
            )
        });
    }

    fn on_scroll(&mut self, x: i32, y: i32, vw: u32, vh: u32, vscroll: f32) {
        let buttons = self.buttons;
        self.with(move |c| c.scroll(x, y, vw as u16, vh as u16, 0.0, vscroll, buttons))
    }

    fn on_key(&mut self, action: u8, keycode: u32, meta: u32) {
        self.with(move |c| c.keycode(action, keycode, 0, meta))
    }

    fn on_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // 服务端的 INJECT_TEXT 是用 KeyCharacterMap 逐字符找按键，CJK 没有对应键位，
        // 只会打印 "Could not inject char u+xxxx" 然后整段丢掉。
        // 所以只要含非 ASCII，就改走「写设备剪贴板 + 触发粘贴」——这是唯一能真正
        // 把中文送进设备的通路（官方 scrcpy 的 Ctrl+V 用的也是它）。
        if crate::control::text_is_typeable(text) {
            self.with(|c| c.text(text));
            return;
        }
        let seq = crate::clipboard::sequence_number() as u64;
        let t0 = std::time::Instant::now();
        let ok = self.with_ok(|c| c.set_clipboard(seq, text, true));
        if crate::window::debug_on() {
            println!("[text] set_clipboard 耗时 {} ms（UI 线程）", t0.elapsed().as_millis());
        }
        if ok {
            println!(
                "[text] 非 ASCII 文本改用剪贴板粘贴（{} 字）",
                text.chars().count()
            );
            self.shared.set_notice("中文已通过「剪贴板粘贴」送入设备");
        } else {
            self.shared.set_notice("输入失败：控制通道不可用");
        }
    }

    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Back => {
                // BACK_OR_SCREEN_ON：设备息屏时先唤醒，比裸发 KEYCODE_BACK 更贴近官方行为
                self.with(|c| c.back_or_screen_on(ACTION_DOWN));
                self.with(|c| c.back_or_screen_on(ACTION_UP));
            }
            Command::Home => {
                self.with(|c| c.keycode(KEY_ACTION_DOWN, KEYCODE_HOME, 0, 0));
                self.with(|c| c.keycode(KEY_ACTION_UP, KEYCODE_HOME, 0, 0));
            }
            Command::Power => {
                self.with(|c| c.keycode(KEY_ACTION_DOWN, KEYCODE_POWER, 0, 0));
                self.with(|c| c.keycode(KEY_ACTION_UP, KEYCODE_POWER, 0, 0));
            }
            Command::ToggleScreen => {
                let next_off = !self.shared.is_screen_off();
                self.shared.set_screen_off(next_off);
                self.with(move |c| c.set_display_power(!next_off));
            }
            Command::Rotate => {
                self.with(|c| c.rotate_device());
            }
            Command::ToggleOverlay => {
                let on = !self.shared.overlay_enabled();
                self.shared.set_overlay_enabled(on);
                if on {
                    println!("[ui] 界面层级叠加：开（每 2 秒 dump 一次，Ctrl+U 关闭）");
                } else {
                    println!("[ui] 界面层级叠加：关");
                }
                self.shared.request_repaint();
            }
            Command::Screenshot => self.screenshot(),
            Command::ToggleRecord => {
                let on = !self.shared.recording.load(Ordering::Relaxed);
                self.shared.recording.store(on, Ordering::Relaxed);
                println!("[rec] 录制：{}", if on { "开始" } else { "停止" });
                self.shared.set_notice(if on {
                    "开始录制…"
                } else {
                    "正在结束录制…"
                });
                self.shared.request_repaint();
            }
            Command::OpenFolder => self.open_folder(),
            Command::ToggleClipboard => {
                let on = !self.shared.clipboard_on.load(Ordering::Relaxed);
                self.shared.clipboard_on.store(on, Ordering::Relaxed);
                println!("[clip] 剪贴板同步：{}", if on { "开" } else { "关" });
                if on {
                    // 打开时先把本机剪贴板推一次，避免「打开了但没有内容」的困惑
                    if let Some(text) = crate::clipboard::read_text() {
                        if !text.is_empty() {
                            let seq = crate::clipboard::sequence_number() as u64;
                            self.with(|c| c.set_clipboard(seq, &text, false));
                        }
                    }
                }
                self.shared.request_repaint();
            }
            Command::ToggleAudio => {
                let on = !self.shared.audio_on.load(Ordering::Relaxed);
                self.shared.audio_on.store(on, Ordering::Relaxed);
                if let Some(a) = &self.audio {
                    a.set_on(on);
                }
                println!("[audio] 播放：{}", if on { "开" } else { "关" });
                self.shared.request_repaint();
            }
            Command::PasteToDevice => {
                // 直接把本机剪贴板连「粘贴动作」一起发给设备：服务端写完剪贴板会自动按 PASTE
                match crate::clipboard::read_text() {
                    Some(text) if !text.is_empty() => {
                        let seq = crate::clipboard::sequence_number() as u64;
                        self.with(|c| c.set_clipboard(seq, &text, true));
                        self.shared.set_notice("已粘贴到设备");
                    }
                    _ => self.shared.set_notice("本机剪贴板没有文本"),
                }
            }
            Command::FetchClipboard(copy_key) => {
                if !self.shared.clipboard_on.load(Ordering::Relaxed) {
                    self.shared
                        .set_notice("先打开「剪贴板同步」才能从设备复制");
                    return;
                }
                println!("[clip] 请求设备剪贴板（copy_key={copy_key}）");
                self.with(|c| c.get_clipboard(copy_key));
            }
            Command::ExpandNotification => {
                self.with(|c| c.expand_notification_panel());
                println!("[ui] 展开通知栏");
            }
            Command::ExpandSettings => {
                self.with(|c| c.expand_settings_panel());
                println!("[ui] 展开快捷设置");
            }
            Command::CollapsePanels => {
                self.with(|c| c.collapse_panels());
                println!("[ui] 收起面板");
            }
            Command::ResetVideo => {
                self.with(|c| c.reset_video());
                println!("[ui] 已请求重置视频编码器");
                self.shared.set_notice("已请求重置视频流");
            }
            Command::StartApp => match self.start_app.clone() {
                Some(pkg) if !pkg.is_empty() => {
                    self.with(|c| c.start_app(&pkg));
                    println!("[ui] 启动应用 {pkg}");
                    self.shared.set_notice(format!("启动应用：{pkg}"));
                }
                _ => self
                    .shared
                    .set_notice("没有配置应用包名，请用 --start-app=<包名> 启动"),
            },
            Command::Reconnect => {
                if self.shared.params_dirty() {
                    println!("[ui] 按新参数重连…");
                    self.shared
                        .reconnect_busy
                        .store(true, Ordering::Relaxed);
                    self.shared.set_notice("正在按新参数重连…");
                    self.shared.reconnect_requested.store(true, Ordering::Relaxed);
                    self.shared.request_repaint();
                } else {
                    self.shared.set_notice("参数没有变化，无需重连");
                }
            }
            Command::Quit => {
                self.shared.quit.store(true, Ordering::Relaxed);
            }
        }
    }

    fn on_key_raw(&mut self, vk: u32, down: bool, mods: u8) {
        self.sync_uhid_epoch();
        // 修饰键自己按下时，GetKeyState 里还可能是旧状态；这里自己维护一份，
        // 保证 Ctrl/Shift 的按下与抬起都能正确反映在报文里
        if let Some(bit) = crate::hid::keyboard::vk_to_mod_bit(vk) {
            if down {
                self.uhid_mods |= bit;
            } else {
                self.uhid_mods &= !bit;
            }
        }
        if !self.ensure_uhid_keyboard() {
            // 设备不支持 UHID（Android 9 以下）或控制通道不可用：
            // 退回 INJECT_KEYCODE，至少单键还能用（组合键在设备内不生效）
            if let Some(kc) = crate::window::vk_to_keycode(vk) {
                let action = if down {
                    crate::control::KEY_ACTION_DOWN
                } else {
                    crate::control::KEY_ACTION_UP
                };
                let meta = crate::window::meta_state();
                self.with(|c| c.keycode(action, kc, 0, meta));
            }
            return;
        }
        let Some(usage) = crate::hid::keyboard::vk_to_usage(vk) else {
            return;
        };
        let report = self.uhid_kb.report(mods | self.uhid_mods, usage, down);
        self.with(|c| c.uhid_input(crate::hid::KEYBOARD_ID, &report));
    }

    fn on_mouse_move(&mut self, dx: i32, dy: i32) {
        self.sync_uhid_epoch();
        if !self.ensure_uhid_mouse() {
            return;
        }
        for rep in crate::hid::mouse::split_moves(self.mouse_buttons, dx, dy) {
            self.with(|c| c.uhid_input(crate::hid::MOUSE_ID, &rep));
        }
    }

    fn on_mouse_button(&mut self, buttons: u8) {
        self.sync_uhid_epoch();
        if !self.ensure_uhid_mouse() {
            return;
        }
        self.mouse_buttons = buttons;
        let rep = crate::hid::mouse::report(self.mouse_buttons, 0, 0, 0);
        self.with(|c| c.uhid_input(crate::hid::MOUSE_ID, &rep));
    }

    fn on_mouse_wheel(&mut self, delta: i32) {
        self.sync_uhid_epoch();
        if !self.ensure_uhid_mouse() {
            return;
        }
        let rep = crate::hid::mouse::report(self.mouse_buttons, 0, 0, delta);
        self.with(|c| c.uhid_input(crate::hid::MOUSE_ID, &rep));
    }

    fn on_files(&mut self, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        println!("[push] 收到 {} 个文件", paths.len());
        self.push_files(paths);
    }
}

/// 读包直到确定分辨率，已读到的包缓存起来交给解码线程
fn read_until_dims(
    video: &mut VideoReader<std::net::TcpStream>,
) -> Result<(u32, u32, Vec<Packet>)> {
    let mut buffered: Vec<Packet> = Vec::new();
    let mut dims: Option<(u32, u32)> = None;

    for _ in 0..2400 {
        let pkt = video.read_packet()?;
        let done = match &pkt {
            Packet::Session { width, height } => {
                if *width > 0 && *height > 0 {
                    dims = Some((*width, *height));
                }
                dims.is_some()
            }
            Packet::Config(data) => {
                if dims.is_none() {
                    if let Ok(d) = sps::parse_dimensions(data) {
                        dims = Some((d.width, d.height));
                    }
                }
                dims.is_some()
            }
            Packet::Frame { data, .. } => {
                if dims.is_none() {
                    if let Ok(d) = sps::parse_dimensions(data) {
                        dims = Some((d.width, d.height));
                    }
                }
                dims.is_some()
            }
        };
        buffered.push(pkt);
        if done {
            break;
        }
    }

    match dims {
        Some((w, h)) => Ok((w, h, buffered)),
        None => Err(Error::Msg(
            "等待视频尺寸超时（既没有 session 包也没有 SPS）".into(),
        )),
    }
}

/// 调试用：--dump-frame 指定的导出路径
static DUMP_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
static FRAME_NO: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn set_dump_path(p: Option<std::path::PathBuf>) {
    if let Some(p) = p {
        let _ = DUMP_PATH.set(p);
    }
}

/// 把一帧 NV12 转成 BGRA 并投递给窗口。
///
/// 面板上的「画面刷新上限」在这里生效：只做时间戳节流，跳过的是**转换 + 上屏**，
/// 解码照常进行（丢包会破坏参考帧直到下一个 IDR，所以不能在这里丢码流）。
fn deliver_frame(shared: &Arc<Shared>, f: &mf::RawFrame) {
    // 用单调时钟做节流（解码线程只有一个，thread_local 足够）
    thread_local! {
        static LAST_RENDER: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    }
    let limit = shared.render_fps();
    if limit > 0 {
        let gap = Duration::from_micros(1_000_000 / limit.max(1) as u64);
        let now = Instant::now();
        let allow = LAST_RENDER.with(|c| {
            // 留 10% 余量，避免源帧率刚好等于上限时抖动成一半帧率
            let ok = match c.get() {
                Some(last) => now.duration_since(last) * 10 >= gap * 9,
                None => true,
            };
            if ok {
                c.set(Some(now));
            }
            ok
        });
        if !allow {
            return;
        }
    }
    let matrix = Matrix::for_resolution(f.width as u32, f.height as u32);
    let (y, uv) = mf::planes(f);
    let mut pixels = shared.take_spare().unwrap_or_default();
    if pixels.len() != f.width * f.height {
        pixels.resize(f.width * f.height, 0);
    }
    nv12_to_bgra(
        &mut pixels,
        y,
        uv,
        f.width,
        f.height,
        f.y_stride,
        f.uv_stride,
        matrix,
    );
    let n = FRAME_NO.fetch_add(1, Ordering::Relaxed);
    if n == 20 {
        if let Some(path) = DUMP_PATH.get() {
            match bmp::write_bgra(path, &pixels, f.width, f.height) {
                Ok(()) => println!("[debug] 已导出第 {n} 帧到 {}", path.display()),
                Err(e) => eprintln!("[debug] 导出帧失败: {e}"),
            }
        }
    }

    shared.push_frame(FrameSurface {
        pixels,
        width: f.width,
        height: f.height,
        arrived: Instant::now(),
    });
}

fn handle_packet(pkt: Packet, decoder: &mut Decoder, shared: &Arc<Shared>) -> Result<()> {
    match pkt {
        Packet::Session { width, height } => {
            if width > 0 && height > 0 {
                shared.set_video_size(width, height);
            }
        }
        Packet::Config(data) => {
            decoder.set_config(&data);
            // 分辨率以 SPS 为准：解码器的输出类型可能被对齐放大（1200x2670 -> 1200x2688），
            // 一旦把这个尺寸发给设备端，PositionMapper 会因为尺寸不等而静默丢弃所有触摸事件
            if let Some(d) = decoder.visible_dims() {
                shared.set_video_size(d.width, d.height);
            }
        }
        Packet::Frame { pts, key, data } => {
            for f in decoder.decode(&data, pts, key)? {
                shared.set_video_size(f.width as u32, f.height as u32);
                deliver_frame(shared, &f);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    mut video: VideoReader<std::net::TcpStream>,
    mut decoder: Decoder,
    shared: Arc<Shared>,
    buffered: Vec<Packet>,
    base_title: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
    disconnected: Arc<std::sync::atomic::AtomicBool>,
    out_dir: std::path::PathBuf,
    rec: Arc<Mutex<Option<crate::record::Recorder>>>,
) {
    struct FpsCounter {
        frames: u32,
        since: Instant,
    }
    let mut fps = FpsCounter {
        frames: 0,
        since: Instant::now(),
    };
    // 最近一个 GOP 的包（(pts, key, data)）
    let mut gop: std::collections::VecDeque<(u64, bool, Vec<u8>)> =
        std::collections::VecDeque::new();
    for pkt in buffered {
        if let Err(e) = handle_packet(pkt, &mut decoder, &shared) {
            eprintln!("[decode] {e}");
        }
    }
    // 延迟探针：设备 PTS 与本地单调时钟的偏移。
    // 恒定 = 只有编码/传输的固定延迟；持续增长 = 我们这边在积压（读得比处理慢）
    let lat_t0 = Instant::now();
    let mut lat_pts0: Option<u64> = None;
    let mut lat_last: i64 = 0;
    loop {
        if shared.quit.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
            break;
        }
        match video.read_packet() {
            Ok(pkt) => {
                let is_frame = matches!(pkt, Packet::Frame { .. });
                let frame_pts = match &pkt {
                    Packet::Frame { pts, .. } => Some(*pts),
                    _ => None,
                };
                if let Packet::Frame { pts, key, data } = &pkt {
                    // 维护一个「最近一个 GOP」的缓存：录制从中途开始时，
                    // 把上一个关键帧之后的数据一起写进去，否则文件里没有 IDR，播不出来
                    if *key {
                        gop.clear();
                    }
                    gop.push_back((*pts, *key, data.clone()));
                    while gop.len() > 900 {
                        gop.pop_front(); // 兜底：别让缓冲无限涨（约 30s 的包）
                    }
                    let cfg = decoder.config().to_vec();
                    record_frame(&shared, &rec, &out_dir, *pts, *key, data, &cfg, &gop);
                }
                if let Err(e) = handle_packet(pkt, &mut decoder, &shared) {
                    eprintln!("[decode] {e}");
                    break;
                }
                if is_frame {
                    if let Some(pts) = frame_pts {
                        let base = *lat_pts0.get_or_insert(pts);
                        let lag = lat_t0.elapsed().as_micros() as i64 - (pts - base) as i64;
                        lat_last = lag;
                    }
                    fps.frames += 1;
                    if fps.since.elapsed() >= Duration::from_secs(1) {
                        let s = fps.since.elapsed().as_secs_f64();
                        let (w, h) = shared.video_size();
                        let rate = fps.frames as f64 / s;
                        // 解码帧率 + 真实上屏帧率一起显示：两者差得多说明瓶颈在绘制
                        let paint = shared.paint_fps.load(Ordering::Relaxed);
                        shared.set_title(&format!(
                            "{base_title} | {w}x{h} @ 解码 {rate:.0}fps / 上屏 {paint}fps"
                        ));
                        if crate::window::debug_on() {
                            println!("[fps] 解码 {rate:.0} fps / 队列滞后 {} ms", lat_last / 1000);
                        }

                        // 积压自愈：滞后超过 1.2s 说明采集帧率一直高于我们的处理能力，
                        // 数据全堆在 socket 里，不清空的话延迟只会越涨越高。
                        // 重建会话会换新 socket，积压自然消失；加 15s 冷却避免反复重连。
                        if lat_last > 1_200_000 {
                            // 顺手把采集帧率降到 60，否则重建后还是会积压，
                            // 变成「每隔十几秒重连一次」的抖动
                            let before = shared.panel_state().pending_fps;
                            if before == 0 || before > 60 {
                                shared.set_pending_fps(60);
                            }
                            eprintln!(
                                "[decode] 画面滞后 {} ms（采集帧率高于解码能力），\
                                 已把采集帧率降到 60 并重建会话以清空积压",
                                lat_last / 1000
                            );
                            shared.set_notice("画面延迟过大，已降到 60fps 并重建会话…");
                            shared.reconnect_requested.store(true, Ordering::Relaxed);
                            break;
                        }
                        fps.frames = 0;
                        fps.since = Instant::now();
                    }
                }
            }
            Err(e) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // 不是用户主动退出：标记断线，让上层退避重连（窗口不销毁）
                eprintln!("[decode] 视频流中断: {e}");
                disconnected.store(true, Ordering::Relaxed);
                // 关键：UI 循环靠 Shared 里的标志决定是否返回，局部标志它看不到
                shared.disconnected.store(true, Ordering::Relaxed);
                shared.set_disconnected(0);
                break;
            }
        }
    }
}

/// 界面层级抓取间隔。uiautomator dump 在真机上单次就要 0.5~2s，
/// 间隔再小只会让命令排队，没有实际收益。
const DUMP_INTERVAL: Duration = Duration::from_secs(2);

/// 界面层级叠加的后台抓取线程。
///
/// 串行执行，任何时刻最多只有一次 dump 在飞；间隔从**上一次结束**开始算 1 秒，
/// 这样即使设备端 dump 变慢也不会越积越多（不做固定频率排队）。
fn spawn_overlay_worker(adb: crate::adb::Adb, serial: String, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut consecutive_failures = 0u32;
        let mut logged = false;

        loop {
            if shared.quit.load(Ordering::Relaxed) {
                break;
            }

            // 关闭状态：清掉快照并把资源还回去，然后低频轮询开关
            if !shared.overlay_enabled() {
                if shared.overlay().is_some() {
                    shared.set_overlay(None);
                    shared.request_repaint();
                }
                std::thread::sleep(Duration::from_millis(120));
                continue;
            }

            let started = Instant::now();
            match crate::uitree::dump(&adb, &serial) {
                Ok(snap) => {
                    if consecutive_failures > 0 {
                        println!("[ui] 层级抓取已恢复");
                    }
                    consecutive_failures = 0;
                    logged = false;
                    let count = snap.nodes.len();
                    shared.set_dump_status(crate::panel::DumpStatus {
                        ok: true,
                        nodes: count,
                        elapsed_ms: started.elapsed().as_millis() as u32,
                        msg: String::new(),
                        at: Some(Instant::now()),
                    });
                    shared.set_overlay(Some(Arc::new(snap)));
                    shared.request_repaint();
                    if crate::window::debug_on() {
                        let (sw, sh) = shared.video_size();
                        println!("[ui] 抓取到 {count} 个节点，画面 {sw}x{sh}");
                    }
                }
                Err(e) => {
                    consecutive_failures += 1;
                    // 只报第一次，避免每秒刷屏（面板里能看到每一次的原因）
                    if !logged {
                        logged = true;
                        eprintln!("[ui] 抓取界面层级失败（重试中）: {e}");
                    }
                    shared.set_dump_status(crate::panel::DumpStatus {
                        ok: false,
                        nodes: 0,
                        elapsed_ms: started.elapsed().as_millis() as u32,
                        msg: e.to_string(),
                        at: Some(Instant::now()),
                    });
                    // 连续失败说明画面已不可读（锁屏/切到安全界面），把旧框线撤掉
                    if consecutive_failures >= 3 && shared.overlay().is_some() {
                        shared.set_overlay(None);
                        shared.request_repaint();
                    }
                }
            }
            // 状态里的「多久之前」要随时间走，哪怕没有新数据也每秒刷一次面板
            shared.request_repaint();

            // 固定 2 秒一次（uiautomator 本身就要 0.5~2s，再密也没有意义），
            // 期间可被退出/关闭打断
            let mut left = DUMP_INTERVAL.saturating_sub(started.elapsed());
            while left > Duration::ZERO {
                if shared.quit.load(Ordering::Relaxed) || !shared.overlay_enabled() {
                    break;
                }
                let step = left.min(Duration::from_millis(100));
                std::thread::sleep(step);
                left -= step;
            }
        }
    });
}

/// 按开关启停录制：只在关键帧上开始，保证文件头可解码
#[allow(clippy::too_many_arguments)]
fn record_frame(
    shared: &Arc<Shared>,
    rec: &Arc<Mutex<Option<crate::record::Recorder>>>,
    out_dir: &std::path::Path,
    pts: u64,
    key: bool,
    data: &[u8],
    seq_header: &[u8],
    gop: &std::collections::VecDeque<(u64, bool, Vec<u8>)>,
) {
    let want = shared.recording.load(Ordering::Relaxed);
    let mut guard = match rec.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if want && guard.is_none() {
        // 不再等关键帧：设备端 i-frame 间隔是 10 秒，等下去用户会以为没在录。
        // 靠 SPS/PPS（sequence header）保证文件头完整，最坏情况是开头一小段花屏。
        let _ = key;
        let (w, h) = shared.video_size();
        let fps = {
            let f = shared.max_fps.load(Ordering::Relaxed);
            if f > 0 {
                f as u32
            } else {
                60
            }
        };
        let path = crate::record::default_path(out_dir, "rec", "mp4");
        match crate::record::Recorder::start(path, w, h, fps, seq_header) {
            Ok(mut r) => {
                // 先把缓存的关键帧及其后续帧补进去，保证文件从 IDR 开始
                let mut written = 0usize;
                for (p, k, d) in gop.iter() {
                    if r.write(d, *p, *k).is_ok() {
                        written += 1;
                    }
                }
                println!(
                    "[rec] 开始录制 {}（补入 {} 个缓存包）",
                    r.path.display(),
                    written
                );
                shared.set_notice(format!("录制中: {}", r.path.display()));
                *guard = Some(r);
            }
            Err(e) => {
                eprintln!("[rec] 无法开始录制: {e}");
                shared.set_notice(format!("录制失败: {e}"));
                shared.recording.store(false, Ordering::Relaxed);
                return;
            }
        }
    }
    if !want {
        if let Some(r) = guard.take() {
            drop(guard);
            match r.finish() {
                Ok((path, n)) => {
                    println!("[rec] 已保存 {}（{} 个访问单元）", path.display(), n);
                    shared.set_notice(format!("录制已保存: {}", path.display()));
                }
                Err(e) => shared.set_notice(format!("录制收尾失败: {e}")),
            }
        }
        return;
    }
    if let Some(r) = guard.as_mut() {
        let _ = r.write(data, pts, key);
    }
}

/// 控制通道的可替换句柄（重连后整体换新）
type ControlSlot = Arc<Mutex<Option<Controller>>>;

/// 设备消息（设备 → 主机）读取线程：目前只有剪贴板和 ack
fn spawn_device_msg_reader(sock: std::net::TcpStream, shared: Arc<Shared>) {
    use std::io::Read;
    std::thread::spawn(move || {
        let mut sock = sock;
        loop {
            if shared.quit.load(Ordering::Relaxed) {
                return;
            }
            let mut kind = [0u8; 1];
            if sock.read_exact(&mut kind).is_err() {
                return;
            }
            match kind[0] {
                0 => {
                    // 剪贴板：len(4) + UTF-8
                    let mut len = [0u8; 4];
                    if sock.read_exact(&mut len).is_err() {
                        return;
                    }
                    let n = (u32::from_be_bytes(len) as usize).min(1 << 20);
                    let mut buf = vec![0u8; n];
                    if n > 0 && sock.read_exact(&mut buf).is_err() {
                        return;
                    }
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if shared.clipboard_on.load(Ordering::Relaxed) && !text.is_empty() {
                        println!("[clip] 设备 → 本机 {} 字符", text.chars().count());
                        crate::clipboard::write_text(&text);
                        // 写入后序号会变，记下来让 watcher 跳过，否则会立刻被推回设备
                        shared
                            .clip_guard
                            .store(crate::clipboard::sequence_number(), Ordering::Relaxed);
                    }
                }
                1 => {
                    // ack_clipboard：sequence(8)
                    let mut seq = [0u8; 8];
                    if sock.read_exact(&mut seq).is_err() {
                        return;
                    }
                }
                2 => {
                    // UHID_OUTPUT：id(2) + size(2) + data（键盘 LED 等），读掉即可，
                    // 不读会让后续消息流错位
                    let mut head = [0u8; 4];
                    if sock.read_exact(&mut head).is_err() {
                        return;
                    }
                    let size = u16::from_be_bytes([head[2], head[3]]) as usize;
                    let mut data = vec![0u8; size.min(64)];
                    if size > 0 && sock.read_exact(&mut data).is_err() {
                        return;
                    }
                }
                other => {
                    eprintln!("[control] 不认识设备消息类型 {other}，停止读取");
                    return;
                }
            }
        }
    });
}

/// 本机剪贴板 → 设备（轮询变化）
fn spawn_clipboard_watcher(slot: ControlSlot, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut last_seq = crate::clipboard::sequence_number();
        let mut last_sent = String::new();
        loop {
            if shared.quit.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
            let seq = crate::clipboard::sequence_number();
            if !shared.clipboard_on.load(Ordering::Relaxed) {
                last_seq = seq; // 关闭期间的变化不补发
                continue;
            }
            if seq == last_seq || seq == shared.clip_guard.load(Ordering::Relaxed) {
                continue;
            }
            last_seq = seq;
            let Some(text) = crate::clipboard::read_text() else {
                continue;
            };
            if text.is_empty() || text == last_sent {
                continue;
            }
            last_sent = text.clone();
            println!("[clip] 本机 → 设备 {} 字符", text.chars().count());
            if let Ok(g) = slot.lock() {
                if let Some(c) = g.as_ref() {
                    let _ = c.set_clipboard(seq as u64, &text, false);
                }
            }
        }
    });
}

/// 音频流读取线程：裸 PCM 直接丢给播放器
fn spawn_audio_reader(
    mut reader: VideoReader<std::net::TcpStream>,
    player: Arc<crate::audio::AudioPlayer>,
    shared: Arc<Shared>,
) {
    std::thread::spawn(move || loop {
        if shared.quit.load(Ordering::Relaxed) {
            return;
        }
        match reader.read_packet() {
            Ok(Packet::Frame { data, .. }) => {
                if shared.audio_on.load(Ordering::Relaxed) {
                    player.push(&data);
                }
            }
            Ok(_) => {}
            Err(_) => return,
        }
    });
}

/// 设备模式：连接 → 建窗 → 后台解码；参数变化时在同一窗口里重建会话
/// 断线重连的退避：1s → 2s → 4s → 8s → 16s → 30s（封顶）
fn next_backoff(cur: u64) -> u64 {
    (cur.saturating_mul(2)).min(30)
}

pub fn run_device(session: crate::server::Session, opts: &crate::cli::Options) -> Result<()> {
    let out_dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("scrcpy-rs-out");
    let _ = std::fs::create_dir_all(&out_dir);

    // 音频播放器全生命周期只建一次（重连不打断音频设备）
    let audio = if opts.audio {
        Some(Arc::new(crate::audio::AudioPlayer::start()))
    } else {
        None
    };
    let ctrl_slot: ControlSlot = Arc::new(Mutex::new(None));
    let mut win: Option<Window> = None;
    let mut cur = opts.clone();
    let mut next_session: Option<crate::server::Session> = Some(session);
    let mut backoff = 1u64;
    // 参数变更触发的重连不用等退避
    let mut skip_wait = false;
    // 两个常驻后台线程只启动一次
    let spawn_workers = std::sync::Once::new();

    loop {
        // ---- 1) 取一代会话；没有就按退避重建 ----
        let mut session = loop {
            if let Some(s) = next_session.take() {
                break s;
            }
            if !skip_wait {
                let secs = backoff;
                match &win {
                    Some(w) => {
                        let sh = w.shared();
                        // 逐秒倒计时，面板能显示「n 秒后重试」，期间可关窗打断
                        for left in (1..=secs).rev() {
                            if sh.quit.load(Ordering::Relaxed) {
                                return Ok(());
                            }
                            sh.set_disconnected(left as u32);
                            std::thread::sleep(Duration::from_secs(1));
                        }
                        if sh.quit.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        sh.set_disconnected(0);
                    }
                    None => std::thread::sleep(Duration::from_secs(secs)),
                }
            }
            skip_wait = false;
            if let Some(w) = &win {
                w.shared().set_disconnected(0);
            }
            let adb = crate::adb::Adb::discover(cur.adb_path.as_deref())?;
            match crate::server::start(adb, &cur) {
                Ok(s) => {
                    backoff = 1;
                    break s;
                }
                Err(e) => {
                    eprintln!("[reconnect] 重连失败（{}s 后重试）: {e}", next_backoff(backoff));
                    if let Some(w) = &win {
                        w.shared().set_notice(format!("重连失败：{e}"));
                    }
                    backoff = next_backoff(backoff);
                }
            }
        };

        // ---- 2) 装好这一代会话 ----
        let mut video = match session.take_video() {
            Some(v) => v,
            None => {
                eprintln!("[video] 视频流已被取走，重建会话");
                next_session = None;
                backoff = next_backoff(backoff);
                continue;
            }
        };
        let (w, h, buffered) = read_until_dims(&mut video)?;
        println!("[video] 分辨率: {w}x{h}");

        if let Some(sock) = session.control.take() {
            let controller = Controller::new(sock)?;
            if let Ok(mut g) = ctrl_slot.lock() {
                *g = Some(controller);
            }
        }

        let title = cur
            .window_title
            .clone()
            .unwrap_or_else(|| format!("{} - scrcpy-rs", session.device_name));

        let shared = match &win {
            Some(w) => w.shared(),
            None => {
                let w = Window::create(&title, w, h, cur.fullscreen)?;
                let s = w.shared();
                s.set_input(Box::new(AppController::new(
                    ctrl_slot.clone(),
                    s.clone(),
                    session.adb(),
                    session.serial.clone(),
                    audio.clone(),
                    out_dir.clone(),
                    cur.start_app.clone(),
                )));
                s.set_active_params(cur.max_size, cur.bit_rate, cur.max_fps.unwrap_or(0.0) as u32);
                win = Some(w);
                s
            }
        };
        let win_ref = win.as_ref().unwrap();
        shared.set_video_size(w, h);
        win_ref.set_title(&format!("{title} | {w}x{h}"));
        shared.reconnect_busy.store(false, Ordering::Relaxed);
        shared.disconnected.store(false, Ordering::Relaxed);
        // 新一代控制通道：UHID 等会话内状态由 AppController 自行作废重建
        shared.bump_control_epoch();
        shared.set_connected();

        // 设备消息读取线程
        if let Ok(g) = ctrl_slot.lock() {
            if let Some(c) = g.as_ref() {
                if let Ok(read_sock) = c.try_clone_stream() {
                    spawn_device_msg_reader(read_sock, shared.clone());
                }
            }
        }

        if cur.turn_screen_off {
            shared.set_screen_off(true);
            shared.with_sink(|s| s.set_display_power(false));
        }

        // 音频
        if let Some(player) = &audio {
            player.set_on(shared.audio_on.load(Ordering::Relaxed));
            shared.audio_ok.store(player.is_ok(), Ordering::Relaxed);
            if let Some(reader) = session.take_audio() {
                spawn_audio_reader(reader, player.clone(), shared.clone());
            }
        }

        // 解码线程（带本代停止标志与断线标志）
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let disconnected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rec_slot: Arc<Mutex<Option<crate::record::Recorder>>> = Arc::new(Mutex::new(None));
        {
            let stop = stop.clone();
            let disc = disconnected.clone();
            let shared2 = shared.clone();
            let title2 = title.clone();
            let out2 = out_dir.clone();
            let rec2 = rec_slot.clone();
            std::thread::spawn(move || {
                let decoder = match Decoder::new() {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("[decode] 解码器初始化失败: {e}");
                        shared2.quit.store(true, Ordering::Relaxed);
                        return;
                    }
                };
                decode_loop(video, decoder, shared2, buffered, title2, stop, disc, out2, rec2);
            });
        }

        if opts.ui_overlay {
            shared.set_overlay_enabled(true);
        }

        // 层级抓取与剪贴板监听只依赖 Shared/adb/serial，与会话无关：
        // 用 Once 保证整个进程只起一次，否则每次重连都会多一对线程
        spawn_workers.call_once(|| {
            spawn_overlay_worker(session.adb(), session.serial.clone(), shared.clone());
            spawn_clipboard_watcher(ctrl_slot.clone(), shared.clone());
        });

        let _ = win_ref.run();

        // ---- 3) 收尾这一代 ----
        stop.store(true, Ordering::Relaxed);
        shared.set_overlay_enabled(false);
        if let Some(rec) = rec_slot.lock().ok().and_then(|mut g| g.take()) {
            if let Ok((path, n)) = rec.finish() {
                println!("[rec] 已保存 {}（{} 个访问单元）", path.display(), n);
            }
        }
        if let Ok(mut g) = ctrl_slot.lock() {
            *g = None;
        }
        session.cleanup();
        drop(session);

        if shared.quit.load(Ordering::Relaxed) {
            return Ok(());
        }

        // 参数变更 → 立即按新参数重建
        if shared.reconnect_requested.swap(false, Ordering::Relaxed) {
            let st = shared.panel_state();
            cur.max_size = st.pending_size;
            cur.bit_rate = st.pending_bitrate;
            cur.max_fps = if st.pending_fps == 0 {
                None
            } else {
                Some(st.pending_fps as f32)
            };
            println!(
                "[ui] 重连：max_size={} bit_rate={} max_fps={:?}",
                cur.max_size, cur.bit_rate, cur.max_fps
            );
            shared.set_active_params(cur.max_size, cur.bit_rate, cur.max_fps.unwrap_or(0.0) as u32);
            shared.reconnect_busy.store(false, Ordering::Relaxed);
            shared.set_notice("已按新参数重连");
            skip_wait = true;
            next_session = None;
            continue;
        }

        // 意外断线 → 退避重连（窗口和面板都保留）
        println!("[reconnect] 连接中断，准备重连");
        shared.set_disconnected(0);
        shared.set_notice("连接中断，正在重连…");
        backoff = 1;
        next_session = None;
    }
}

/// 文件模式：直接播放 Annex-B 裸流，用于没有设备时自测整条渲染链路
pub fn run_file(path: &std::path::Path, opts: &crate::cli::Options) -> Result<()> {
    let data = std::fs::read(path)?;
    if data.is_empty() {
        return Err(Error::Msg("文件为空".into()));
    }
    let dims = sps::parse_dimensions(&data)?;
    println!("[file] 解析到分辨率 {}x{}", dims.width, dims.height);

    let win = Window::create(
        &format!("{} - scrcpy-rs (file)", path.display()),
        dims.width,
        dims.height,
        opts.fullscreen,
    )?;
    let shared = win.shared();
    shared.set_video_size(dims.width, dims.height);

    let aus = split_access_units(&data);
    println!("[file] 共 {} 个访问单元", aus.len());
    let config = extract_config(&data);

    let shared2 = shared.clone();
    std::thread::spawn(move || {
        let mut decoder = match Decoder::new() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[file] 解码器初始化失败: {e}");
                shared2.quit.store(true, Ordering::Relaxed);
                return;
            }
        };
        if let Some(cfg) = config {
            decoder.set_config(&cfg);
        }
        let start = Instant::now();
        let mut pushed = 0usize;
        for (idx, (range, key)) in aus.iter().enumerate() {
            if shared2.quit.load(Ordering::Relaxed) {
                return;
            }
            let pts = idx as u64 * 16_666;
            match decoder.decode(&data[range.clone()], pts, *key) {
                Ok(frames) => {
                    for f in &frames {
                        deliver_frame(&shared2, f);
                        pushed += 1;
                    }
                }
                Err(e) => eprintln!("[file] 解码失败: {e}"),
            }
            let target = Duration::from_micros(pts + 16_666);
            let elapsed = start.elapsed();
            if target > elapsed {
                std::thread::sleep(target - elapsed);
            }
        }
        println!("[file] 播放结束，共输出 {pushed} 帧");
    });

    win.run()
}

struct Nal {
    nal_type: u8,
    /// 起始码偏移
    sc: usize,
    /// NAL 头偏移
    data: usize,
    end: usize,
}

fn find_nals(data: &[u8]) -> Vec<Nal> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut start: Option<usize> = None;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                if end > s {
                    out.push(Nal {
                        nal_type: data[s] & 0x1F,
                        sc: sc_start(data, s),
                        data: s,
                        end,
                    });
                }
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if s < data.len() {
            let mut end = data.len();
            while end > s && data[end - 1] == 0 {
                end -= 1;
            }
            out.push(Nal {
                nal_type: data[s] & 0x1F,
                sc: sc_start(data, s),
                data: s,
                end,
            });
        }
    }
    out
}

/// 找到 NAL 负载之前的起始码起点（3 字节或 4 字节）
fn sc_start(data: &[u8], payload: usize) -> usize {
    if payload >= 4 && data[payload - 4..payload - 1] == [0, 0, 0] {
        payload - 4
    } else if payload >= 3 {
        payload - 3
    } else {
        0
    }
}

/// 切成访问单元：返回 (字节区间, 是否关键帧)
fn split_access_units(data: &[u8]) -> Vec<(std::ops::Range<usize>, bool)> {
    let nals = find_nals(data);
    let mut out: Vec<(std::ops::Range<usize>, bool)> = Vec::new();
    let mut cur_start: Option<usize> = None;
    let mut cur_end = 0usize;
    let mut cur_key = false;
    let mut cur_has_vcl = false;

    for nal in nals {
        let is_vcl = nal.nal_type == 1 || nal.nal_type == 5;
        // 图片边界：当前 AU 里已经有 VCL，又遇到「新图片的首个 slice」或 AUD
        let starts_picture = if is_vcl {
            nal.data + 1 < data.len() && data[nal.data + 1] & 0x80 != 0
        } else {
            nal.nal_type == 9
        };

        if starts_picture && cur_has_vcl {
            out.push((cur_start.take().unwrap()..cur_end, cur_key));
            cur_key = false;
            cur_has_vcl = false;
        }
        if cur_start.is_none() {
            cur_start = Some(nal.sc);
        }
        cur_end = nal.end;
        if nal.nal_type == 5 {
            cur_key = true;
        }
        if is_vcl {
            cur_has_vcl = true;
        }
    }
    if let Some(s) = cur_start {
        out.push((s..cur_end, cur_key));
    }
    out
}

/// 抓出文件里的 SPS/PPS 作为解码器配置
fn extract_config(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for nal in find_nals(data) {
        if nal.nal_type == 7 || nal.nal_type == 8 {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[nal.data..nal.end]);
            if out.len() > 8192 {
                break;
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}
