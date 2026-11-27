//! NV12 -> BGRA 色彩转换。按行分块并行，纯整数运算。
//!
//! 输出缓冲区每个像素是 u32，内存字节序为 B,G,R,A（即 0xAARRGGBB），
//! 正好匹配 Windows 的 32bpp BI_RGB DIB。

/// BT.601 有限范围系数（标清）与 BT.709（高清）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Matrix {
    Bt601,
    Bt709,
}

impl Matrix {
    pub fn for_resolution(width: u32, height: u32) -> Matrix {
        // 宽高都 >= 720 时按 BT.709，否则 BT.601（与 Android 编码器惯例一致）
        if width >= 1280 || height >= 720 {
            Matrix::Bt709
        } else {
            Matrix::Bt601
        }
    }
}

#[inline(always)]
fn clamp_u8(v: i32) -> u32 {
    if v < 0 {
        0
    } else if v > 255 {
        255
    } else {
        v as u32
    }
}

/// NV12（Y 平面 + 交错 UV 平面）转 BGRA
pub fn nv12_to_bgra(
    dst: &mut [u32],
    y: &[u8],
    uv: &[u8],
    width: usize,
    height: usize,
    y_stride: usize,
    uv_stride: usize,
    matrix: Matrix,
) {
    assert!(dst.len() >= width * height);

    // 防御：解码器给的缓冲可能比可见区域小（异常情况），宁可少画几行也不要越界 panic
    let rows_y = if y_stride == 0 { 0 } else { y.len() / y_stride };
    let rows_uv = if uv_stride == 0 { 0 } else { uv.len() / uv_stride };
    let height = height.min(rows_y).min(rows_uv * 2);
    if height == 0 || width == 0 {
        return;
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4)
        .max(1);
    let rows_per_chunk = height.div_ceil(threads).max(1);
    let dst = &mut dst[..width * height];

    let (ky_r, ku_g, ku_b, kv_r, kv_g) = match matrix {
        Matrix::Bt601 => (298, -100, 516, 409, -208),
        Matrix::Bt709 => (298, -55, 541, 459, -136),
    };

    std::thread::scope(|s| {
        for (chunk_idx, chunk) in dst.chunks_mut(rows_per_chunk * width).enumerate() {
            let y0 = chunk_idx * rows_per_chunk;
            let rows = chunk.len() / width;
            if rows == 0 {
                continue;
            }
            let y = &y[..];
            let uv = &uv[..];
            s.spawn(move || {
                for row in 0..rows {
                    let sy = y0 + row;
                    let yrow = &y[sy * y_stride..sy * y_stride + width];
                    let uvrow = &uv[(sy / 2) * uv_stride..(sy / 2) * uv_stride + width];
                    let out = &mut chunk[row * width..(row + 1) * width];
                    for x in 0..width {
                        let yv = yrow[x] as i32 - 16;
                        // NV12 的 UV 平面是 U(Cb) 在前、V(Cr) 在后交错排列
                        let idx = x & !1;
                        let u = uvrow[idx] as i32 - 128;
                        let v = uvrow[idx + 1] as i32 - 128;
                        let yy = ky_r * yv + 128;
                        let r = clamp_u8((yy + kv_r * v) >> 8);
                        let g = clamp_u8((yy + ku_g * u + kv_g * v) >> 8);
                        let b = clamp_u8((yy + ku_b * u) >> 8);
                        out[x] = 0xFF00_0000 | (r << 16) | (g << 8) | b;
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_and_white() {
        let w = 4;
        let h = 2;
        let y = vec![16u8; w * h]; // 有限范围黑
        let uv = vec![128u8; w * h / 2];
        let mut dst = vec![0u32; w * h];
        nv12_to_bgra(&mut dst, &y, &uv, w, h, w, w, Matrix::Bt601);
        for px in &dst {
            assert_eq!(*px & 0x00FF_FFFF, 0);
        }

        let y = vec![235u8; w * h]; // 有限范围白
        nv12_to_bgra(&mut dst, &y, &uv, w, h, w, w, Matrix::Bt601);
        for px in &dst {
            let r = (*px >> 16) & 0xFF;
            let g = (*px >> 8) & 0xFF;
            let b = *px & 0xFF;
            assert!(r > 250 && g > 250 && b > 250, "{r} {g} {b}");
        }
    }

    /// 行跨度大于宽度时必须按 stride 取行，而不是按 width 紧排
    #[test]
    fn padded_stride() {
        let (w, h) = (2usize, 2usize);
        let stride = 4usize; // 每行多出 2 字节填充
        // Y=81 / U=90 / V=240 在 BT.601 下约为纯红
        let mut y = vec![0u8; stride * h];
        for row in 0..h {
            y[row * stride] = 81;
            y[row * stride + 1] = 81;
        }
        let mut uv = vec![0u8; stride * (h / 2)];
        uv[0] = 90;
        uv[1] = 240;

        let mut dst = vec![0u32; w * h];
        nv12_to_bgra(&mut dst, &y, &uv, w, h, stride, stride, Matrix::Bt601);
        for px in &dst {
            let r = (*px >> 16) & 0xFF;
            let g = (*px >> 8) & 0xFF;
            let b = *px & 0xFF;
            assert!(r > 250 && g < 5 && b < 5, "期望纯红, 实际 {r},{g},{b}");
        }
    }
}
