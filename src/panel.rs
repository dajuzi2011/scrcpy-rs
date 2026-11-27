//! 视频右侧的控制面板：纯 GDI 自绘 + 命中测试（不用子窗口控件）。
//!
//! 采用「行模型」：所有控件先描述成 `Row`，再由同一份布局算矩形，
//! 绘制和命中测试共用这份布局，所以加控件不需要动两处、也不会错位。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, SelectObject, SetBkMode,
    SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DRAW_TEXT_FORMAT,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    FF_DONTCARE, OUT_DEFAULT_PRECIS, TRANSPARENT, HDC, HFONT,
};

/// 面板默认宽度（物理像素），可用左侧分隔条拖动调整
pub const DEFAULT_W: i32 = 300;
pub const MIN_W: i32 = 240;
pub const MAX_W: i32 = 460;
const MIN_CLIENT_W: i32 = 520;
/// 分隔条热区宽度（以面板左边缘为中心，左右各占一半）
pub const HANDLE_W: i32 = 10;
const PAD: i32 = 16;

/// 画面刷新上限档位（0 = 不限制）
pub const FPS_STEPS: [u32; 5] = [30, 60, 90, 120, 0];
/// 采集分辨率档位（长边，0 = 原画）
pub const SIZE_STEPS: [u32; 5] = [480, 720, 1080, 1440, 0];
/// 码率档位（bit/s）
pub const BITRATE_STEPS: [u32; 5] = [2_000_000, 4_000_000, 8_000_000, 16_000_000, 32_000_000];
/// 采集帧率档位（0 = 不限制）
pub const CAPFPS_STEPS: [u32; 4] = [30, 60, 90, 0];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Screenshot,
    Record,
    OpenFolder,
    Overlay,
    Clipboard,
    Paste,
    Copy,
    Audio,
    UhidKeyboard,
    UhidMouse,
    Smooth,
    FpsDown,
    FpsUp,
    MaxSizeDown,
    MaxSizeUp,
    BitrateDown,
    BitrateUp,
    MaxFpsDown,
    MaxFpsUp,
    Reconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Screenshot,
    Record,
    OpenFolder,
    Overlay,
    Clipboard,
    Paste,
    Copy,
    Audio,
    UhidKeyboard,
    UhidMouse,
    Smooth,
    FpsDown,
    FpsUp,
    MaxSizeDown,
    MaxSizeUp,
    BitrateDown,
    BitrateUp,
    MaxFpsDown,
    MaxFpsUp,
    Reconnect,
}

impl Item {
    pub fn action(self) -> Action {
        match self {
            Item::Screenshot => Action::Screenshot,
            Item::Record => Action::Record,
            Item::OpenFolder => Action::OpenFolder,
            Item::Overlay => Action::Overlay,
            Item::Clipboard => Action::Clipboard,
            Item::Paste => Action::Paste,
            Item::Copy => Action::Copy,
            Item::Audio => Action::Audio,
            Item::UhidKeyboard => Action::UhidKeyboard,
            Item::UhidMouse => Action::UhidMouse,
            Item::Smooth => Action::Smooth,
            Item::FpsDown => Action::FpsDown,
            Item::FpsUp => Action::FpsUp,
            Item::MaxSizeDown => Action::MaxSizeDown,
            Item::MaxSizeUp => Action::MaxSizeUp,
            Item::BitrateDown => Action::BitrateDown,
            Item::BitrateUp => Action::BitrateUp,
            Item::MaxFpsDown => Action::MaxFpsDown,
            Item::MaxFpsUp => Action::MaxFpsUp,
            Item::Reconnect => Action::Reconnect,
        }
    }

    pub fn code(self) -> u32 {
        self as u32 + 1
    }

    pub fn from_code(c: u32) -> Option<Item> {
        const ALL: [Item; 20] = [
            Item::Screenshot,
            Item::Record,
            Item::OpenFolder,
            Item::Overlay,
            Item::Clipboard,
            Item::Paste,
            Item::Copy,
            Item::Audio,
            Item::UhidKeyboard,
            Item::UhidMouse,
            Item::Smooth,
            Item::FpsDown,
            Item::FpsUp,
            Item::MaxSizeDown,
            Item::MaxSizeUp,
            Item::BitrateDown,
            Item::BitrateUp,
            Item::MaxFpsDown,
            Item::MaxFpsUp,
            Item::Reconnect,
        ];
        if c == 0 {
            None
        } else {
            ALL.get(c as usize - 1).copied()
        }
    }
}

/// 每次抓取的结果，面板上直接显示
#[derive(Debug, Clone, Default)]
pub struct DumpStatus {
    pub ok: bool,
    pub nodes: usize,
    pub elapsed_ms: u32,
    pub msg: String,
    pub at: Option<std::time::Instant>,
}

/// 面板要显示的运行时状态（由 window.rs 从 Shared 里取）
#[derive(Debug, Clone, Default)]
pub struct State {
    pub overlay_on: bool,
    pub recording: bool,
    pub clipboard_on: bool,
    pub audio_on: bool,
    pub uhid_keyboard: bool,
    pub uhid_mouse: bool,
    pub smooth_scale: bool,
    pub fps_limit: u32,
    /// 当前会话已生效的参数
    pub max_size: u32,
    pub bitrate: u32,
    pub max_fps: u32,
    /// 面板上待应用的参数
    pub pending_size: u32,
    pub pending_bitrate: u32,
    pub pending_fps: u32,
    pub reconnect_busy: bool,
    pub hover: Option<Item>,
    pub handle_hot: bool,
    /// 一行提示（截图已保存 / 推送中 / 错误…）
    pub notice: String,
    /// 连接状态（已连接 / 断开重连中）
    pub session: String,
    pub dump: DumpStatus,
}

const fn rgb(r: u32, g: u32, b: u32) -> COLORREF {
    COLORREF(r | (g << 8) | (b << 16))
}

mod color {
    use super::rgb;
    use windows::Win32::Foundation::COLORREF;
    pub const BG: COLORREF = rgb(26, 28, 32);
    pub const CARD: COLORREF = rgb(38, 41, 47);
    pub const CARD_HOVER: COLORREF = rgb(52, 56, 64);
    pub const DIVIDER: COLORREF = rgb(52, 56, 63);
    pub const LABEL: COLORREF = rgb(140, 147, 158);
    pub const TEXT: COLORREF = rgb(228, 232, 238);
    pub const ACCENT: COLORREF = rgb(64, 158, 255);
    pub const OK: COLORREF = rgb(99, 224, 107);
    pub const FAIL: COLORREF = rgb(255, 110, 110);
    pub const WARN: COLORREF = rgb(255, 196, 92);
}

pub struct Fonts {
    pub normal: HFONT,
    pub bold: HFONT,
    pub small: HFONT,
}

impl Fonts {
    pub fn create(dpi: u32) -> Self {
        let scale = |px: i32| -> i32 { (px as f32 * dpi as f32 / 96.0).round() as i32 };
        let make = |px: i32, weight: i32| unsafe {
            let face: Vec<u16> = "Microsoft YaHei UI\0".encode_utf16().collect();
            CreateFontW(
                -scale(px),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                FF_DONTCARE.0 as u32,
                PCWSTR(face.as_ptr()),
            )
        };
        Self {
            normal: make(14, 400),
            bold: make(15, 700),
            small: make(12, 400),
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        unsafe {
            for f in [self.normal, self.bold, self.small] {
                let _ = DeleteObject(f);
            }
        }
    }
}

pub fn resolve_width(cw: i32, pref: i32) -> i32 {
    if cw < MIN_CLIENT_W {
        return 0;
    }
    let max_by_window = (cw - 280).max(MIN_W);
    pref.clamp(MIN_W, MAX_W).min(max_by_window)
}

pub fn origin_x(cw: i32, pw: i32) -> i32 {
    cw - pw
}

pub fn on_handle(cw: i32, pw: i32, x: i32) -> bool {
    pw > 0 && (x - origin_x(cw, pw)).abs() <= HANDLE_W / 2
}

// ---------- 行模型 ----------

enum Row {
    Section(&'static str),
    /// 两个按钮并排
    Pair(Item, &'static str, Item, &'static str),
    Toggle(Item, &'static str, bool),
    Stepper {
        label: &'static str,
        down: Item,
        up: Item,
        value: String,
    },
    Button(Item, &'static str),
    Info(String),
}

const H_SECTION: i32 = 30;
const H_PAIR: i32 = 38;
const H_TOGGLE: i32 = 36;
const H_STEPPER: i32 = 36;
const H_BUTTON: i32 = 36;
const H_INFO: i32 = 20;
const GAP: i32 = 6;

fn rows(st: &State) -> Vec<Row> {
    let dirty = st.pending_size != st.max_size
        || st.pending_bitrate != st.bitrate
        || st.pending_fps != st.max_fps;
    vec![
        Row::Section("画面"),
        Row::Pair(
            Item::Screenshot,
            "截图",
            Item::Record,
            if st.recording { "停止录制" } else { "录制" },
        ),
        Row::Pair(
            Item::OpenFolder,
            "打开目录",
            Item::Audio,
            if st.audio_on { "音频 开" } else { "音频 关" },
        ),
        Row::Stepper {
            label: "刷新上限",
            down: Item::FpsDown,
            up: Item::FpsUp,
            value: fps_text(st.fps_limit),
        },
        Row::Section("控制"),
        Row::Toggle(Item::Overlay, "层级叠加", st.overlay_on),
        Row::Toggle(Item::Clipboard, "剪贴板同步", st.clipboard_on),
        Row::Pair(Item::Paste, "粘贴到设备", Item::Copy, "从设备复制"),
        Row::Toggle(Item::UhidKeyboard, "UHID 键盘", st.uhid_keyboard),
        Row::Toggle(Item::UhidMouse, "相对鼠标", st.uhid_mouse),
        Row::Toggle(Item::Smooth, "平滑缩放", st.smooth_scale),
        Row::Info("把文件拖进窗口即可推送到手机".into()),
        Row::Section("采集参数（改完需重连）"),
        Row::Stepper {
            label: "分辨率",
            down: Item::MaxSizeDown,
            up: Item::MaxSizeUp,
            value: size_text(st.pending_size),
        },
        Row::Stepper {
            label: "码率",
            down: Item::BitrateDown,
            up: Item::BitrateUp,
            value: bitrate_text(st.pending_bitrate),
        },
        Row::Stepper {
            label: "帧率",
            down: Item::MaxFpsDown,
            up: Item::MaxFpsUp,
            value: capfps_text(st.pending_fps),
        },
        Row::Button(
            Item::Reconnect,
            if st.reconnect_busy {
                "正在重连…"
            } else if dirty {
                "应用并重连"
            } else {
                "重连"
            },
        ),
        Row::Section("状态"),
    ]
}

/// 计算每一行的矩形，同时产出可点区域表
fn layout(cw: i32, pw: i32, st: &State) -> (Vec<(Row, RECT)>, Vec<(Item, RECT)>) {
    let x0 = origin_x(cw, pw) + PAD;
    let inner = pw - 2 * PAD;
    let mut y = 30;
    let mut out = Vec::new();
    let mut hits = Vec::new();
    for row in rows(st) {
        let h = match &row {
            Row::Section(_) => H_SECTION,
            Row::Pair(..) => H_PAIR,
            Row::Toggle(..) => H_TOGGLE,
            Row::Stepper { .. } => H_STEPPER,
            Row::Button(..) => H_BUTTON,
            Row::Info(_) => H_INFO,
        };
        let rc = rect(x0, y, x0 + inner, y + h);
        match &row {
            Row::Pair(a, _, b, _) => {
                let half = (inner - 8) / 2;
                hits.push((*a, rect(x0, y, x0 + half, y + h)));
                hits.push((*b, rect(x0 + half + 8, y, x0 + inner, y + h)));
            }
            Row::Toggle(i, _, _) => hits.push((*i, rc)),
            Row::Stepper { down, up, .. } => {
                // 必须和 draw 里的位置完全一致：−/+ 画在标签右侧和行尾
                hits.push((*down, stepper_down_rect(rc)));
                hits.push((*up, stepper_up_rect(rc)));
            }
            Row::Button(i, _) => hits.push((*i, rc)),
            _ => {}
        }
        out.push((row, rc));
        y += h + GAP;
    }
    (out, hits)
}

pub fn hit(cw: i32, pw: i32, x: i32, y: i32, st: &State) -> Option<Item> {
    if pw == 0 || x < origin_x(cw, pw) {
        return None;
    }
    let (_, hits) = layout(cw, pw, st);
    hits.into_iter()
        .find(|(_, rc)| x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom)
        .map(|(i, _)| i)
}

/// 步进器「−」按钮的矩形（绘制与命中共用）
fn stepper_down_rect(rc: RECT) -> RECT {
    rect(rc.left + 78, rc.top, rc.left + 114, rc.bottom)
}

/// 步进器「+」按钮的矩形
fn stepper_up_rect(rc: RECT) -> RECT {
    rect(rc.right - 36, rc.top, rc.right, rc.bottom)
}

fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

fn fill(hdc: HDC, rc: &RECT, color: COLORREF) {
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(hdc, rc, brush);
        let _ = DeleteObject(brush);
    }
}

fn draw_text(
    hdc: HDC,
    font: HFONT,
    s: &str,
    rc: RECT,
    color: COLORREF,
    align: DRAW_TEXT_FORMAT,
    ellipsis: bool,
) {
    // 空字符串必须提前返回：windows-rs 对空 slice 传的是悬垂指针 + cchText=0，
    // GDI 内部仍会去读首字符，直接访问违例（这里踩过）
    if s.is_empty() {
        return;
    }
    let mut buf: Vec<u16> = s.encode_utf16().collect();
    let mut rc = rc;
    let mut flags = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | align;
    if ellipsis {
        flags |= DT_END_ELLIPSIS;
    }
    unsafe {
        SelectObject(hdc, font);
        SetTextColor(hdc, color);
        SetBkMode(hdc, TRANSPARENT);
        let _ = DrawTextW(hdc, &mut buf, &mut rc, flags);
    }
}

fn fps_text(v: u32) -> String {
    if v == 0 {
        "不限".into()
    } else {
        format!("{v} fps")
    }
}

fn size_text(v: u32) -> String {
    if v == 0 {
        "原画".into()
    } else {
        format!("{v} p")
    }
}

fn bitrate_text(v: u32) -> String {
    format!("{} Mbps", v / 1_000_000)
}

fn capfps_text(v: u32) -> String {
    if v == 0 {
        "不限".into()
    } else {
        format!("{v}")
    }
}

pub fn draw(hdc: HDC, cw: i32, ch: i32, pw: i32, fonts: &Fonts, st: &State) {
    if pw == 0 {
        return;
    }
    let x0 = origin_x(cw, pw);
    let l = x0 + PAD;
    let r = cw - PAD;

    fill(hdc, &rect(x0, 0, cw, ch), color::BG);
    if st.handle_hot {
        fill(hdc, &rect(x0, 0, x0 + 3, ch), color::ACCENT);
    } else {
        fill(hdc, &rect(x0, 0, x0 + 1, ch), color::DIVIDER);
    }

    draw_text(
        hdc,
        fonts.bold,
        "控制面板",
        rect(l, 2, r, 26),
        color::TEXT,
        DT_LEFT,
        false,
    );

    let (rows, _) = layout(cw, pw, st);
    for (row, rc) in rows {
        let hovered = |i: Item| st.hover == Some(i);
        match row {
            Row::Section(name) => {
                fill(
                    hdc,
                    &rect(rc.left, rc.top + 16, rc.right, rc.top + 17),
                    color::DIVIDER,
                );
                draw_text(
                    hdc,
                    fonts.small,
                    name,
                    rect(rc.left, rc.top + 1, rc.right, rc.top + 17),
                    color::LABEL,
                    DT_LEFT,
                    false,
                );
            }
            Row::Info(text) => {
                draw_text(
                    hdc,
                    fonts.small,
                    &text,
                    rect(rc.left, rc.top, rc.right, rc.bottom),
                    color::LABEL,
                    DT_LEFT,
                    true,
                );
            }
            Row::Pair(a, la, b, lb) => {
                let half = (rc.right - rc.left - 8) / 2;
                for (item, label, x) in [(a, la, rc.left), (b, lb, rc.left + half + 8)] {
                    let item_rc = rect(x, rc.top, x + half, rc.bottom);
                    fill(
                        hdc,
                        &item_rc,
                        if hovered(item) {
                            color::CARD_HOVER
                        } else {
                            color::CARD
                        },
                    );
                    draw_text(
                        hdc,
                        fonts.normal,
                        label,
                        item_rc,
                        color::TEXT,
                        DT_CENTER,
                        false,
                    );
                }
            }
            Row::Toggle(item, label, on) => {
                fill(
                    hdc,
                    &rc,
                    if hovered(item) {
                        color::CARD_HOVER
                    } else {
                        color::CARD
                    },
                );
                fill(
                    hdc,
                    &rect(rc.left, rc.top, rc.left + 3, rc.bottom),
                    if on { color::ACCENT } else { color::DIVIDER },
                );
                draw_text(
                    hdc,
                    fonts.normal,
                    label,
                    rect(rc.left + 14, rc.top, rc.right - 60, rc.bottom),
                    color::TEXT,
                    DT_LEFT,
                    false,
                );
                draw_text(
                    hdc,
                    fonts.small,
                    if on { "开启" } else { "关闭" },
                    rect(rc.right - 58, rc.top, rc.right - 12, rc.bottom),
                    if on { color::OK } else { color::LABEL },
                    DT_RIGHT,
                    false,
                );
            }
            Row::Stepper {
                label,
                down,
                up,
                value,
            } => {
                draw_text(
                    hdc,
                    fonts.small,
                    label,
                    rect(rc.left, rc.top, rc.left + 76, rc.bottom),
                    color::LABEL,
                    DT_LEFT,
                    false,
                );
                let d = stepper_down_rect(rc);
                let u = stepper_up_rect(rc);
                let v = rect(d.right + 4, rc.top, u.left - 4, rc.bottom);
                for (item, sym, item_rc) in [(down, "−", d), (up, "+", u)] {
                    fill(
                        hdc,
                        &item_rc,
                        if hovered(item) {
                            color::CARD_HOVER
                        } else {
                            color::CARD
                        },
                    );
                    draw_text(
                        hdc,
                        fonts.normal,
                        sym,
                        item_rc,
                        color::TEXT,
                        DT_CENTER,
                        false,
                    );
                }
                fill(hdc, &v, color::CARD);
                draw_text(hdc, fonts.normal, &value, v, color::TEXT, DT_CENTER, false);
            }
            Row::Button(item, label) => {
                let enabled = !st.reconnect_busy;
                fill(
                    hdc,
                    &rc,
                    if hovered(item) && enabled {
                        color::ACCENT
                    } else if enabled {
                        color::CARD_HOVER
                    } else {
                        color::CARD
                    },
                );
                draw_text(
                    hdc,
                    fonts.normal,
                    label,
                    rc,
                    if enabled { color::TEXT } else { color::LABEL },
                    DT_CENTER,
                    false,
                );
            }
        }
    }

    // 提示 / 抓取状态（贴底，避免和上面的行重叠）
    let mut y = (ch - 76).max(0);
    if !st.notice.is_empty() {
        draw_text(
            hdc,
            fonts.small,
            &st.notice,
            rect(l, y, r, y + 18),
            color::WARN,
            DT_LEFT,
            true,
        );
        y += 20;
    }
    if !st.session.is_empty() {
        draw_text(
            hdc,
            fonts.small,
            &st.session,
            rect(l, y, r, y + 18),
            if st.session.starts_with("已连接") {
                color::OK
            } else {
                color::FAIL
            },
            DT_LEFT,
            true,
        );
        y += 20;
    }
    let (l1, l2, c2) = status_lines(&st.dump);
    draw_text(
        hdc,
        fonts.small,
        &l1,
        rect(l, y, r, y + 18),
        color::LABEL,
        DT_LEFT,
        true,
    );
    draw_text(
        hdc,
        fonts.small,
        &l2,
        rect(l, y + 18, r, y + 36),
        c2,
        DT_LEFT,
        true,
    );
}

fn status_lines(s: &DumpStatus) -> (String, String, COLORREF) {
    let Some(at) = s.at else {
        return ("层级：尚未抓取".into(), String::new(), color::LABEL);
    };
    let age = at.elapsed().as_secs_f32();
    if s.ok {
        (
            format!(
                "层级：{} 节点 · {} ms · {:.0}s 前",
                s.nodes, s.elapsed_ms, age
            ),
            "成功".into(),
            color::OK,
        )
    } else {
        (
            format!("层级：{:.0}s 前失败 · {} ms", age, s.elapsed_ms),
            s.msg.clone(),
            color::FAIL,
        )
    }
}

/// 在档位表上挪一格：`up=false` 往低调
pub fn step_value(steps: &[u32], cur: u32, up: bool) -> u32 {
    match steps.iter().position(|&v| v == cur) {
        Some(i) => {
            if up {
                steps[(i + 1).min(steps.len() - 1)]
            } else {
                steps[i.saturating_sub(1)]
            }
        }
        // 不在表里（例如命令行传了奇怪的值）：落到第一档
        None => steps[0],
    }
}

pub fn step_fps(cur: u32, up: bool) -> u32 {
    step_value(&FPS_STEPS, cur, up)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> State {
        State {
            fps_limit: 60,
            max_size: 0,
            bitrate: 8_000_000,
            max_fps: 0,
            pending_size: 0,
            pending_bitrate: 8_000_000,
            pending_fps: 0,
            ..Default::default()
        }
    }

    #[test]
    fn hit_matches_layout() {
        let cw = 1000;
        let pw = resolve_width(cw, DEFAULT_W);
        let st = st();
        let (_, hits) = layout(cw, pw, &st);
        assert!(!hits.is_empty());
        for (item, rc) in hits {
            let cx = (rc.left + rc.right) / 2;
            let cy = (rc.top + rc.bottom) / 2;
            assert_eq!(hit(cw, pw, cx, cy, &st), Some(item), "{item:?} 中心应命中");
        }
        assert_eq!(hit(cw, pw, origin_x(cw, pw) - 10, 100, &st), None);
    }

    /// 步进器的命中区域必须覆盖画出来的 − / + 按钮
    #[test]
    fn stepper_hit_matches_drawn_buttons() {
        let cw = 1000;
        let pw = resolve_width(cw, DEFAULT_W);
        let st = st();
        let (rows, _) = layout(cw, pw, &st);
        let mut checked = 0;
        for (row, rc) in rows {
            if let Row::Stepper { down, up, .. } = row {
                let d = stepper_down_rect(rc);
                let u = stepper_up_rect(rc);
                assert_eq!(hit(cw, pw, (d.left + d.right) / 2, (d.top + d.bottom) / 2, &st), Some(down));
                assert_eq!(hit(cw, pw, (u.left + u.right) / 2, (u.top + u.bottom) / 2, &st), Some(up));
                checked += 1;
            }
        }
        assert!(checked >= 4, "应该有 4 个步进器");
    }

    #[test]
    fn narrow_window_has_no_panel() {
        let cw = 400;
        let pw = resolve_width(cw, DEFAULT_W);
        assert_eq!(pw, 0);
        assert_eq!(hit(cw, pw, cw - 10, 100, &st()), None);
    }

    #[test]
    fn width_clamps() {
        assert_eq!(resolve_width(1000, 100), MIN_W);
        assert_eq!(resolve_width(1000, 999), MAX_W);
        assert_eq!(resolve_width(1000, 300), 300);
        assert_eq!(resolve_width(600, MAX_W), 320);
        assert!(on_handle(1000, 300, 1000 - 300));
        assert!(!on_handle(1000, 300, 1000 - 300 - 40));
    }

    #[test]
    fn stepping_stays_in_range() {
        assert_eq!(step_fps(60, true), 90);
        assert_eq!(step_fps(60, false), 30);
        assert_eq!(step_fps(30, false), 30);
        assert_eq!(step_fps(0, true), 0);
        assert_eq!(step_fps(0, false), 120);
        assert_eq!(step_value(&SIZE_STEPS, 0, false), 1440);
        assert_eq!(step_value(&BITRATE_STEPS, 8_000_000, true), 16_000_000);
        assert_eq!(step_value(&CAPFPS_STEPS, 0, false), 90);
        assert_eq!(step_value(&FPS_STEPS, 77, true), 30);
    }

    #[test]
    fn item_codes_roundtrip() {
        let all = [
            Item::Screenshot,
            Item::Record,
            Item::OpenFolder,
            Item::Overlay,
            Item::Clipboard,
            Item::Audio,
            Item::UhidKeyboard,
            Item::UhidMouse,
            Item::FpsDown,
            Item::FpsUp,
            Item::MaxSizeDown,
            Item::MaxSizeUp,
            Item::BitrateDown,
            Item::BitrateUp,
            Item::MaxFpsDown,
            Item::MaxFpsUp,
            Item::Reconnect,
        ];
        for i in all {
            assert_eq!(Item::from_code(i.code()), Some(i));
        }
        assert_eq!(Item::from_code(0), None);
        assert_eq!(Item::from_code(99), None);
    }
}
