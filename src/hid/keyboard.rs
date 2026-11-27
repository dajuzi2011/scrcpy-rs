//! UHID 键盘：8 字节标准引导键盘报文（mods + 保留 + 6 个按键）。

/// HID 报告描述符：标准键盘，6 键无冲 + 5 个 LED
pub const REPORT_DESC: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x05, 0x07, //   Usage Page (Keyboard/Keypad)
    0x19, 0xE0, //   Usage Minimum (224: LeftControl)
    0x29, 0xE7, //   Usage Maximum (231: Right GUI)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x08, //   Report Count (8)
    0x81, 0x02, //   Input (Data,Var,Abs) —— 修饰键字节
    0x95, 0x01, //   Report Count (1)
    0x75, 0x08, //   Report Size (8)
    0x81, 0x01, //   Input (Const) —— 保留字节
    0x95, 0x06, //   Report Count (6)
    0x75, 0x08, //   Report Size (8)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x65, //   Logical Maximum (101)
    0x19, 0x00, //   Usage Minimum (0)
    0x29, 0x65, //   Usage Maximum (101)
    0x81, 0x00, //   Input (Data,Array) —— 6 个按键槽
    0x05, 0x08, //   Usage Page (LEDs)
    0x19, 0x01, //   Usage Minimum (1)
    0x29, 0x05, //   Usage Maximum (5)
    0x95, 0x05, //   Report Count (5)
    0x75, 0x01, //   Report Size (1)
    0x91, 0x02, //   Output (Data,Var,Abs) —— LED
    0x95, 0x01, //   Report Count (1)
    0x75, 0x03, //   Report Size (3)
    0x91, 0x01, //   Output (Const) —— 补齐到 1 字节
    0xC0, // End Collection
];

/// 修饰键位（报文第 0 字节）
pub const MOD_LEFT_CTRL: u8 = 0x01;
pub const MOD_LEFT_SHIFT: u8 = 0x02;
pub const MOD_LEFT_ALT: u8 = 0x04;
pub const MOD_LEFT_GUI: u8 = 0x08;

/// Windows 虚拟键 -> HID usage id。
/// 只映射常用键（字母数字、F1-F12、方向、编辑、符号），够覆盖日常输入与快捷键。
pub fn vk_to_usage(vk: u32) -> Option<u8> {
    let usage = match vk {
        0x41..=0x5A => 0x04 + (vk - 0x41) as u8,      // A-Z
        // 数字键 1..0 对应 usage 0x1E..0x27（注意顺序：1 在 0 前面）
        0x31..=0x39 => 0x1E + (vk - 0x31) as u8,      // 1-9
        0x30 => 0x27,                                  // 0
        0x0D => 0x28,                                  // Enter
        0x1B => 0x29,                                  // Esc
        0x08 => 0x2A,                                  // Backspace
        0x09 => 0x2B,                                  // Tab
        0x20 => 0x2C,                                  // Space
        0xBD => 0x2D,                                  // -
        0xBB => 0x2E,                                  // =
        0xDB => 0x2F,                                  // [
        0xDD => 0x30,                                  // ]
        0xDC => 0x31,                                  // backslash
        0xBA => 0x33,                                  // ;
        0xDE => 0x34,                                  // '
        0xC0 => 0x35,                                  // `
        0xBC => 0x36,                                  // ,
        0xBE => 0x37,                                  // .
        0xBF => 0x38,                                  // /
        0x14 => 0x39,                                  // CapsLock
        0x70..=0x7B => 0x3A + (vk - 0x70) as u8,      // F1-F12
        0x2D => 0x49,                                  // Insert
        0x24 => 0x4A,                                  // Home
        0x21 => 0x4B,                                  // PageUp
        0x2E => 0x4C,                                  // Delete
        0x23 => 0x4D,                                  // End
        0x22 => 0x4E,                                  // PageDown
        0x27 => 0x4F,                                  // Right
        0x25 => 0x50,                                  // Left
        0x28 => 0x51,                                  // Down
        0x26 => 0x52,                                  // Up
        0x90 => 0x53,                                  // NumLock
        // 修饰键（同时会写进 mods 字节，这里给出 usage 以便追踪按下状态）
        0xA2 => 0xE0,                                  // LeftCtrl
        0xA0 => 0xE1,                                  // LeftShift
        0xA4 => 0xE2,                                  // LeftAlt
        0x5B => 0xE3,                                  // LeftGui
        0xA3 => 0xE4,                                  // RightCtrl
        0xA1 => 0xE5,                                  // RightShift
        0xA5 => 0xE6,                                  // RightAlt
        0x5C => 0xE7,                                  // RightGui
        _ => return None,
    };
    Some(usage)
}

/// Windows 虚拟键本身是修饰键时，返回它在 mods 字节里的位
pub fn vk_to_mod_bit(vk: u32) -> Option<u8> {
    let bit = match vk {
        0xA2 | 0x11 => MOD_LEFT_CTRL,   // LeftCtrl / Ctrl
        0xA0 | 0x10 => MOD_LEFT_SHIFT,  // LeftShift / Shift
        0xA4 | 0x12 => MOD_LEFT_ALT,    // LeftAlt / Alt
        0x5B => MOD_LEFT_GUI,           // LeftGui
        _ => return None,
    };
    Some(bit)
}

/// 键盘状态：维护 6 个按键槽，产出完整报文
#[derive(Default)]
pub struct Keyboard {
    keys: [u8; 6],
}

impl Keyboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 按下/抬起一个 usage，返回要发送的 8 字节报文。
    /// `mods` 由调用方按当前修饰键状态算好（保持无状态更简单）。
    pub fn report(&mut self, mods: u8, usage: u8, down: bool) -> [u8; 8] {
        if down {
            if !self.keys.contains(&usage) {
                if let Some(slot) = self.keys.iter_mut().find(|k| **k == 0) {
                    *slot = usage;
                }
            }
        } else {
            for slot in self.keys.iter_mut() {
                if *slot == usage {
                    *slot = 0;
                }
            }
        }
        self.build(mods)
    }

    pub fn build(&self, mods: u8) -> [u8; 8] {
        let mut r = [0u8; 8];
        r[0] = mods;
        r[2..8].copy_from_slice(&self.keys);
        r
    }

    /// 全部松开（切走/重连时避免按键卡住）
    pub fn release_all(&mut self) -> [u8; 8] {
        self.keys = [0; 6];
        self.build(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits() {
        assert_eq!(vk_to_usage(0x41), Some(0x04)); // A
        assert_eq!(vk_to_usage(0x5A), Some(0x1D)); // Z
        assert_eq!(vk_to_usage(0x31), Some(0x1E)); // 1
        assert_eq!(vk_to_usage(0x30), Some(0x27)); // 0
        assert_eq!(vk_to_usage(0x70), Some(0x3A)); // F1
        assert_eq!(vk_to_usage(0x7B), Some(0x45)); // F12
        assert_eq!(vk_to_usage(0xFE), None);
    }

    #[test]
    fn report_layout_and_rollover() {
        let mut kb = Keyboard::new();
        let r = kb.report(MOD_LEFT_CTRL, 0x04, true); // Ctrl+A
        assert_eq!(r[0], MOD_LEFT_CTRL);
        assert_eq!(r[1], 0);
        assert_eq!(&r[2..8], &[0x04, 0, 0, 0, 0, 0]);

        let r = kb.report(MOD_LEFT_CTRL, 0x05, true); // 再按 B
        assert_eq!(&r[2..8], &[0x04, 0x05, 0, 0, 0, 0]);

        let r = kb.report(0, 0x04, false); // 抬起 A
        assert_eq!(&r[2..8], &[0, 0x05, 0, 0, 0, 0]);

        let r = kb.release_all();
        assert_eq!(r, [0u8; 8]);
    }

    #[test]
    fn mods_bits() {
        assert_eq!(vk_to_mod_bit(0xA2), Some(MOD_LEFT_CTRL));
        assert_eq!(vk_to_mod_bit(0xA0), Some(MOD_LEFT_SHIFT));
        assert_eq!(vk_to_mod_bit(0x41), None);
    }

    #[test]
    fn descriptor_starts_with_keyboard_usage() {
        assert_eq!(&REPORT_DESC[0..4], &[0x05, 0x01, 0x09, 0x06]);
        assert_eq!(*REPORT_DESC.last().unwrap(), 0xC0);
    }
}
