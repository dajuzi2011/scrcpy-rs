//! UHID 鼠标：5 字节相对移动报文（buttons + dx + dy + wheel + pan）。
//!
//! 相对模式的意义：绝对坐标模式（现有触摸注入）下，鼠标一动设备指针就「跳」到对应位置，
//! 快速移动会撞到窗口边界；相对模式只发位移增量，和真实鼠标完全一致，游戏/拖拽更跟手。

/// HID 报告描述符：3 键 + 相对 X/Y + 滚轮 + 横向滚轮（AC Pan）
pub const REPORT_DESC: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Buttons)
    0x19, 0x01, //     Usage Minimum (1)
    0x29, 0x03, //     Usage Maximum (3)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x03, //     Report Count (3)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x95, 0x01, //     Report Count (1)
    0x75, 0x05, //     Report Size (5)
    0x81, 0x01, //     Input (Const) —— padding
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x06, //     Input (Data,Var,Rel)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data,Var,Rel)
    0x05, 0x0C, //     Usage Page (Consumer)
    0x0A, 0x38, 0x02, // Usage (AC Pan)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data,Var,Rel)
    0xC0, //   End Collection
    0xC0, // End Collection
];

pub const BTN_LEFT: u8 = 0x01;
pub const BTN_RIGHT: u8 = 0x02;
pub const BTN_MIDDLE: u8 = 0x04;

/// 单次位移最大 127，超出必须拆包（HID 是 8 位有符号）
pub const MAX_DELTA: i32 = 127;

#[inline]
fn clamp_i8(v: i32) -> i8 {
    v.clamp(-127, 127) as i8
}

/// 构造一个 5 字节报文
pub fn report(buttons: u8, dx: i32, dy: i32, wheel: i32) -> [u8; 5] {
    let mut r = [0u8; 5];
    r[0] = buttons;
    r[1] = clamp_i8(dx) as u8;
    r[2] = clamp_i8(dy) as u8;
    r[3] = clamp_i8(wheel) as u8;
    r[4] = 0; // AC Pan 暂不使用
    r
}

/// 把一次大位移拆成多个报文（每个轴最多 127）
pub fn split_moves(buttons: u8, dx: i32, dy: i32) -> Vec<[u8; 5]> {
    let mut out = Vec::new();
    let mut rest_x = dx;
    let mut rest_y = dy;
    loop {
        if rest_x == 0 && rest_y == 0 {
            break;
        }
        let step_x = rest_x.clamp(-MAX_DELTA, MAX_DELTA);
        let step_y = rest_y.clamp(-MAX_DELTA, MAX_DELTA);
        out.push(report(buttons, step_x, step_y, 0));
        rest_x -= step_x;
        rest_y -= step_y;
        if out.len() >= 8 {
            // 再多的位移丢掉，避免一次卡顿后刷屏
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_clamps_and_packs() {
        let r = report(BTN_LEFT, 10, -20, 1);
        assert_eq!(r[0], BTN_LEFT);
        assert_eq!(r[1] as i8, 10);
        assert_eq!(r[2] as i8, -20);
        assert_eq!(r[3] as i8, 1);

        let big = report(0, 300, -300, 0);
        assert_eq!(big[1] as i8, 127);
        assert_eq!(big[2] as i8, -127);
    }

    #[test]
    fn split_moves_covers_delta() {
        let packs = split_moves(BTN_RIGHT, 300, -300);
        assert_eq!(packs.len(), 3);
        let sum_x: i32 = packs.iter().map(|p| p[1] as i8 as i32).sum();
        let sum_y: i32 = packs.iter().map(|p| p[2] as i8 as i32).sum();
        assert_eq!(sum_x, 300);
        assert_eq!(sum_y, -300);
    }

    #[test]
    fn zero_move_produces_nothing() {
        assert!(split_moves(0, 0, 0).is_empty());
    }
}
