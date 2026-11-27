//! Windows 剪贴板读写（CF_UNICODETEXT），以及变化检测。
//!
//! 变化检测用 `GetClipboardSequenceNumber()` 轮询而不是 `AddClipboardFormatListener`：
//! 后者要求有一个窗口来收 `WM_CLIPBOARDUPDATE`，而我们已经有自己的消息循环，
//! 再挂一套监听反而更容易出错；轮询每 200ms 一次，开销可以忽略。

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

pub const CF_UNICODETEXT_FLAG: u32 = 13;

/// 读剪贴板文本；没有文本时返回 None
pub fn read_text() -> Option<String> {
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT_FLAG).is_err() {
            return None;
        }
        if OpenClipboard(HWND::default()).is_err() {
            return None;
        }
        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT_FLAG).ok()?;
            if handle.is_invalid() {
                return None;
            }
            let ptr = GlobalLock(HGLOBAL(handle.0)) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0usize;
            while *ptr.add(len) != 0 && len < 1_000_000 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let text = String::from_utf16_lossy(slice);
            let _ = GlobalUnlock(HGLOBAL(handle.0));
            Some(text)
        })();
        let _ = CloseClipboard();
        result
    }
}

/// 写剪贴板文本（会清空其它格式）
pub fn write_text(text: &str) -> bool {
    if text.len() > 4 * 1024 * 1024 {
        return false; // 超大内容不推，避免卡死
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return false;
        }
        let ok = (|| -> bool {
            if EmptyClipboard().is_err() {
                return false;
            }
            let h = match GlobalAlloc(GMEM_MOVEABLE, bytes) {
                Ok(h) => h,
                Err(_) => return false,
            };
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                return false;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            let _ = GlobalUnlock(h);
            if SetClipboardData(CF_UNICODETEXT_FLAG, HANDLE(h.0)).is_err() {
                return false;
            }
            true
        })();
        let _ = CloseClipboard();
        ok
    }
}

pub fn sequence_number() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_text() {
        // 会动用户的剪贴板，所以只在能打开时做，且恢复不了就算了（测试环境专用）
        let marker = format!("scrcpy-rs-clip-{}", std::process::id());
        if !write_text(&marker) {
            return; // 剪贴板被占用时跳过
        }
        assert_eq!(read_text().as_deref(), Some(marker.as_str()));
    }
}
