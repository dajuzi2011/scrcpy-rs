//! 极小的 BMP 写出（32bpp，无压缩），只用于把解码结果落盘做自测/排障。
//!
//! 选 BMP 而不是 PNG 是因为它不需要任何第三方压缩库。

use std::io::Write;
use std::path::Path;

use crate::error::Result;

/// 写出 32bpp BGRA BMP（`pixels` 是自上而下的 0xAARRGGBB）
pub fn write_bgra(path: &Path, pixels: &[u32], width: usize, height: usize) -> Result<()> {
    assert!(pixels.len() >= width * height);
    let row_bytes = width * 4;
    let image_size = row_bytes * height;
    let file_size = 14 + 40 + image_size;

    let mut buf = Vec::with_capacity(file_size);
    // BITMAPFILEHEADER
    buf.extend_from_slice(b"BM");
    buf.extend_from_slice(&(file_size as u32).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&54u32.to_le_bytes());
    // BITMAPINFOHEADER
    buf.extend_from_slice(&40u32.to_le_bytes());
    buf.extend_from_slice(&(width as i32).to_le_bytes());
    buf.extend_from_slice(&(height as i32).to_le_bytes()); // 正数 = 自下而上
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&32u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    buf.extend_from_slice(&(image_size as u32).to_le_bytes());
    buf.extend_from_slice(&2835i32.to_le_bytes()); // 72 DPI
    buf.extend_from_slice(&2835i32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());

    // 像素：BMP 要求自下而上
    for y in (0..height).rev() {
        let row = &pixels[y * width..(y + 1) * width];
        for px in row {
            // 内存里 u32 的小端字节序正好是 B,G,R,A
            buf.extend_from_slice(&px.to_le_bytes());
        }
    }

    let mut f = std::fs::File::create(path)?;
    f.write_all(&buf)?;
    Ok(())
}
