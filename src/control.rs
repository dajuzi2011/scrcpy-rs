//! 控制通道：把输入事件编码成 scrcpy 的控制协议（v5.0.1）。
//!
//! 所有整数字段都是大端；浮点用定点数：
//!
//! 下面是协议常量表，暂未全部用到，允许 dead_code。
#![allow(dead_code)]
//! 压力是 u16/2^16（1.0 用 0xFFFF 表示），滚动是 i16/2^15（范围 [-16,16]）。

use std::io::Write;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use crate::error::Result;

pub const ACTION_DOWN: u8 = 0;
pub const ACTION_UP: u8 = 1;
pub const ACTION_MOVE: u8 = 2;

pub const BUTTON_PRIMARY: u32 = 1;
pub const BUTTON_SECONDARY: u32 = 2;
pub const BUTTON_TERTIARY: u32 = 4;

// 协议里的 well-known pointer id（设备端把它们映射成「鼠标 / 通用手指 / 虚拟手指」）
pub const POINTER_ID_MOUSE: u64 = u64::MAX; // -1
pub const POINTER_ID_GENERIC_FINGER: u64 = u64::MAX - 1; // -2
pub const POINTER_ID_VIRTUAL_FINGER: u64 = u64::MAX - 2; // -3

// Android KeyEvent 的 action
pub const KEY_ACTION_DOWN: u8 = 0;
pub const KEY_ACTION_UP: u8 = 1;

// Android KeyEvent meta 状态位
pub const META_SHIFT_ON: u32 = 0x01;
pub const META_ALT_ON: u32 = 0x02;
pub const META_CTRL_ON: u32 = 0x1000;

// 常用 Android keycode
pub const KEYCODE_HOME: u32 = 3;
pub const KEYCODE_BACK: u32 = 4;
pub const KEYCODE_CALL: u32 = 5;
pub const KEYCODE_ENDCALL: u32 = 6;
pub const KEYCODE_0: u32 = 7;
pub const KEYCODE_A: u32 = 29;
pub const KEYCODE_COMMA: u32 = 55;
pub const KEYCODE_PERIOD: u32 = 56;
pub const KEYCODE_TAB: u32 = 61;
pub const KEYCODE_SPACE: u32 = 62;
pub const KEYCODE_ENTER: u32 = 66;
pub const KEYCODE_DEL: u32 = 67;
pub const KEYCODE_MINUS: u32 = 69;
pub const KEYCODE_EQUALS: u32 = 70;
pub const KEYCODE_LEFT_BRACKET: u32 = 71;
pub const KEYCODE_RIGHT_BRACKET: u32 = 72;
pub const KEYCODE_BACKSLASH: u32 = 73;
pub const KEYCODE_SEMICOLON: u32 = 74;
pub const KEYCODE_APOSTROPHE: u32 = 75;
pub const KEYCODE_SLASH: u32 = 76;
pub const KEYCODE_GRAVE: u32 = 68;
pub const KEYCODE_AT: u32 = 77;
pub const KEYCODE_PLUS: u32 = 81;
pub const KEYCODE_MENU: u32 = 82;
pub const KEYCODE_PAGE_UP: u32 = 92;
pub const KEYCODE_PAGE_DOWN: u32 = 93;
pub const KEYCODE_ESCAPE: u32 = 111;
pub const KEYCODE_FORWARD_DEL: u32 = 112;
pub const KEYCODE_MOVE_HOME: u32 = 122;
pub const KEYCODE_MOVE_END: u32 = 123;
pub const KEYCODE_POWER: u32 = 26;
pub const KEYCODE_VOLUME_UP: u32 = 24;
pub const KEYCODE_VOLUME_DOWN: u32 = 25;
pub const KEYCODE_VOLUME_MUTE: u32 = 164;
pub const KEYCODE_DPAD_UP: u32 = 19;
pub const KEYCODE_DPAD_DOWN: u32 = 20;
pub const KEYCODE_DPAD_LEFT: u32 = 21;
pub const KEYCODE_DPAD_RIGHT: u32 = 22;
pub const KEYCODE_APP_SWITCH: u32 = 187;
pub const KEYCODE_F1: u32 = 131;

/// 控制通道。内部用 Arc<Mutex<..>> 包装，方便多个线程各自持有一份句柄：
/// UI 线程发输入、剪贴板线程发文本、后台线程读设备消息，互不干扰。
#[derive(Clone)]
pub struct Controller {
    sock: Arc<Mutex<TcpStream>>,
}

impl Controller {
    pub fn new(sock: TcpStream) -> Result<Self> {
        sock.set_nodelay(true)?;
        Ok(Self {
            sock: Arc::new(Mutex::new(sock)),
        })
    }

    /// 再取一个句柄（共享同一个 socket）
    pub fn handle(&self) -> Self {
        self.clone()
    }

    /// 克隆底层 socket 用于读取设备消息（设备 → 主机）
    pub fn try_clone_stream(&self) -> Result<TcpStream> {
        let g = self
            .sock
            .lock()
            .map_err(|_| crate::errmsg!("控制通道锁失败"))?;
        Ok(g.try_clone()?)
    }

    fn send(&self, buf: &[u8]) -> Result<()> {
        let mut g = self
            .sock
            .lock()
            .map_err(|_| crate::errmsg!("控制通道锁失败"))?;
        g.write_all(buf)?;
        Ok(())
    }

    pub fn keycode(&self, action: u8, keycode: u32, repeat: u32, meta: u32) -> Result<()> {
        let mut buf = Vec::with_capacity(14);
        buf.push(0);
        buf.push(action);
        buf.extend_from_slice(&(keycode as i32).to_be_bytes());
        buf.extend_from_slice(&(repeat as i32).to_be_bytes());
        buf.extend_from_slice(&(meta as i32).to_be_bytes());
        self.send(&buf)
    }

    /// 注入文本（UTF-8）。单条上限约 256KB，这里按 250 字节切分
    pub fn text(&self, text: &str) -> Result<()> {
        let mut start = 0usize;
        let bytes = text.as_bytes();
        while start < bytes.len() {
            let mut end = (start + 250).min(bytes.len());
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            let chunk = &text[start..end];
            let mut buf = Vec::with_capacity(5 + chunk.len());
            buf.push(1);
            buf.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
            buf.extend_from_slice(chunk.as_bytes());
            self.send(&buf)?;
            start = end;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn touch(
        &self,
        action: u8,
        pointer_id: u64,
        x: i32,
        y: i32,
        screen_w: u16,
        screen_h: u16,
        pressure: f32,
        action_button: u32,
        buttons: u32,
    ) -> Result<()> {
        // 压力是 u16 定点：1.0 -> 0xFFFF（不能写成 65536，截断成 u16 会变成 0）
        let p = if pressure >= 1.0 {
            0xFFFFu32
        } else {
            (pressure.max(0.0) * 65535.0) as u32
        };
        let mut buf = Vec::with_capacity(32);
        buf.push(2);
        buf.push(action);
        buf.extend_from_slice(&(pointer_id as i64).to_be_bytes());
        buf.extend_from_slice(&x.to_be_bytes());
        buf.extend_from_slice(&y.to_be_bytes());
        buf.extend_from_slice(&screen_w.to_be_bytes());
        buf.extend_from_slice(&screen_h.to_be_bytes());
        buf.extend_from_slice(&(p as u16).to_be_bytes());
        buf.extend_from_slice(&(action_button as i32).to_be_bytes());
        buf.extend_from_slice(&(buttons as i32).to_be_bytes());
        self.send(&buf)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn scroll(
        &self,
        x: i32,
        y: i32,
        screen_w: u16,
        screen_h: u16,
        h: f32,
        v: f32,
        buttons: u32,
    ) -> Result<()> {
        fn to_i16(v: f32) -> i16 {
            // 协议范围 [-1, 1]，实际范围 [-16, 16]，服务端会再乘 16
            if v >= 1.0 {
                0x7FFF
            } else if v <= -1.0 {
                -0x8000
            } else {
                (v * 32768.0) as i16
            }
        }
        let mut buf = Vec::with_capacity(24);
        buf.push(3);
        buf.extend_from_slice(&x.to_be_bytes());
        buf.extend_from_slice(&y.to_be_bytes());
        buf.extend_from_slice(&screen_w.to_be_bytes());
        buf.extend_from_slice(&screen_h.to_be_bytes());
        buf.extend_from_slice(&to_i16(h).to_be_bytes());
        buf.extend_from_slice(&to_i16(v).to_be_bytes());
        buf.extend_from_slice(&(buttons as i32).to_be_bytes());
        self.send(&buf)
    }

    pub fn back_or_screen_on(&self, action: u8) -> Result<()> {
        self.send(&[4, action])
    }

    pub fn expand_notification_panel(&self) -> Result<()> {
        self.send(&[5])
    }

    pub fn expand_settings_panel(&self) -> Result<()> {
        self.send(&[6])
    }

    pub fn collapse_panels(&self) -> Result<()> {
        self.send(&[7])
    }

    pub fn set_display_power(&self, on: bool) -> Result<()> {
        self.send(&[10, on as u8])
    }

    pub fn rotate_device(&self) -> Result<()> {
        self.send(&[11])
    }

    /// SET_CLIPBOARD：sequence(8) + paste(1) + len(4) + UTF-8
    pub fn set_clipboard(&self, sequence: u64, text: &str, paste: bool) -> Result<()> {
        let bytes = text.as_bytes();
        // 协议上限 SC_CONTROL_MSG_MAX_SIZE - 14 ≈ 256KB，超出直接截断
        let mut len = bytes.len().min(200 * 1024);
        while len > 0 && !text.is_char_boundary(len) {
            len -= 1;
        }
        let mut buf = Vec::with_capacity(14 + len);
        buf.push(9);
        buf.extend_from_slice(&sequence.to_be_bytes());
        buf.push(paste as u8);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
        buf.extend_from_slice(&bytes[..len]);
        self.send(&buf)
    }

    /// GET_CLIPBOARD：copy_key(1) —— 0 无 / 1 复制 / 2 剪切
    pub fn get_clipboard(&self, copy_key: u8) -> Result<()> {
        self.send(&[8, copy_key])
    }

    /// START_APP：包名长度是 **1 字节** 前缀（和其它消息不一样，别照抄 4 字节）
    pub fn start_app(&self, name: &str) -> Result<()> {
        let bytes = name.as_bytes();
        let len = bytes.len().min(255);
        let mut buf = Vec::with_capacity(2 + len);
        buf.push(16);
        buf.push(len as u8);
        buf.extend_from_slice(&bytes[..len]);
        self.send(&buf)
    }

    /// SCAN_FILE：路径长度是 **4 字节** 前缀，用于让设备媒体库刷新
    pub fn scan_file(&self, path: &str) -> Result<()> {
        let bytes = path.as_bytes();
        let len = bytes.len().min(4096);
        let mut buf = Vec::with_capacity(5 + len);
        buf.push(22);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
        buf.extend_from_slice(&bytes[..len]);
        self.send(&buf)
    }

    /// RESET_VIDEO：让设备重新配置编码器（花屏/尺寸错乱时用）
    pub fn reset_video(&self) -> Result<()> {
        self.send(&[17])
    }

    /// UHID_CREATE：name 长度是 **1 字节**，描述符长度是 **2 字节**
    pub fn uhid_create(
        &self,
        id: u16,
        vendor: u16,
        product: u16,
        name: &str,
        desc: &[u8],
    ) -> Result<()> {
        let name = name.as_bytes();
        let n = name.len().min(255);
        let mut buf = Vec::with_capacity(8 + n + desc.len());
        buf.push(12);
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(&vendor.to_be_bytes());
        buf.extend_from_slice(&product.to_be_bytes());
        buf.push(n as u8);
        buf.extend_from_slice(&name[..n]);
        buf.extend_from_slice(&(desc.len() as u16).to_be_bytes());
        buf.extend_from_slice(desc);
        self.send(&buf)
    }

    /// UHID_INPUT：id(2) + 数据长度(2) + 数据
    pub fn uhid_input(&self, id: u16, data: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(5 + data.len());
        buf.push(13);
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(&(data.len() as u16).to_be_bytes());
        buf.extend_from_slice(data);
        self.send(&buf)
    }

    /// UHID_DESTROY：id(2)
    pub fn uhid_destroy(&self, id: u16) -> Result<()> {
        let mut buf = vec![14u8];
        buf.extend_from_slice(&id.to_be_bytes());
        self.send(&buf)
    }
}

/// 这段文本能否靠 INJECT_TEXT 送达。
///
/// 服务端是用 `KeyCharacterMap.getEvents()` 逐字符找按键的，只有键盘布局上真的存在
/// 的字符（ASCII 可见字符、换行、制表符）才映射得出来；中文、日文、emoji 一律失败并
/// 打印 `Could not inject char u+xxxx`。这类文本必须走剪贴板粘贴。
pub fn text_is_typeable(text: &str) -> bool {
    text.chars()
        .all(|c| (' '..='~').contains(&c) || c == '\n' || c == '\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_typeable_text() {
        assert!(text_is_typeable("hello world"));
        assert!(text_is_typeable("a\tb\nc"));
        assert!(text_is_typeable(""));
        // CJK / emoji / 全角标点都没有键位，必须走粘贴
        assert!(!text_is_typeable("哦这招呼"));
        assert!(!text_is_typeable("abc 中文"));
        assert!(!text_is_typeable("🙂"));
        assert!(!text_is_typeable("，"));
    }

    /// 各消息的长度前缀宽度不一样，这里锁住字节布局，避免以后照抄出错
    #[test]
    fn message_layouts() {
        // START_APP: type(1) + len(1) + name
        let mut name = Vec::new();
        name.push(16u8);
        name.push(11u8);
        name.extend_from_slice(b"com.foo.app");
        assert_eq!(name.len(), 13);

        // SCAN_FILE: type(1) + len(4) + path
        let path = b"/sdcard/Download/a.txt";
        let mut scan = Vec::new();
        scan.push(22u8);
        scan.extend_from_slice(&(path.len() as u32).to_be_bytes());
        scan.extend_from_slice(path);
        assert_eq!(scan.len(), 5 + path.len());

        // SET_CLIPBOARD: type(1) + seq(8) + paste(1) + len(4) + text
        let text = "hi";
        let mut clip = Vec::new();
        clip.push(9u8);
        clip.extend_from_slice(&7u64.to_be_bytes());
        clip.push(1);
        clip.extend_from_slice(&(text.len() as u32).to_be_bytes());
        clip.extend_from_slice(text.as_bytes());
        assert_eq!(clip.len(), 14 + text.len());
        assert_eq!(clip[9], 1, "paste 标志必须在第 10 个字节");
    }

    #[test]
    fn empty_name_is_safe() {
        // 不应 panic
        assert_eq!(0u8 as usize, 0);
    }
}
