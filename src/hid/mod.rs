//! HID 层：把 Windows 的键鼠事件翻译成 HID 报文，供 UHID 通道使用。
//!
//! 为什么需要它：`INJECT_KEYCODE` 只能发单个 keycode（meta 也有限），设备内
//! `Ctrl+A`/`Ctrl+C` 这类组合键不会生效；UHID 是「模拟一个真实 USB 键盘」，
//! 组合键、长按、输入法行为都和插了物理键盘一致。

pub mod keyboard;
pub mod mouse;

/// 键盘 UHID 设备 id（服务端按 id 区分多个虚拟设备）
pub const KEYBOARD_ID: u16 = 1;
/// 鼠标 UHID 设备 id
pub const MOUSE_ID: u16 = 2;
