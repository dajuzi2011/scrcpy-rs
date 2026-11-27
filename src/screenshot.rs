//! 截图：把当前帧的 BGRA 像素用 WIC 编码成 PNG。
//!
//! 为什么用 WIC 而不是自己写：PNG 需要 zlib，而 WIC 是系统自带的图像编解码框架，
//! 一行 COM 调用就能出标准 PNG（带压缩），比手写 deflate 省几百行且产物更小。

use std::path::Path;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::IPropertyBag2;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use crate::error::{Error, Result};


pub struct WicEncoder {
    factory: IWICImagingFactory,
}

impl WicEncoder {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| Error::Msg(format!("WIC 初始化失败（系统缺少图像组件？）: {e}")))?;
            Ok(Self { factory })
        }
    }

    /// 把自上而下的 BGRA 像素写成 PNG
    pub fn write_png(&self, path: &Path, pixels: &[u32], width: usize, height: usize) -> Result<()> {
        if width == 0 || height == 0 || pixels.len() < width * height {
            return Err(Error::Msg("帧数据不完整，无法截图".into()));
        }
        let stride = (width * 4) as u32;
        {
            let result = self.encode_inner(path, pixels, width, height, stride);
            if result.is_err() {
                let _ = std::fs::remove_file(path);
            }
            result
        }
    }

    fn encode_inner(
        &self,
        path: &Path,
        pixels: &[u32],
        width: usize,
        height: usize,
        stride: u32,
    ) -> Result<()> {
        unsafe {
            let stream = self.factory.CreateStream()?;
            let path_w = HSTRING::from(path.as_os_str());
            stream.InitializeFromFilename(PCWSTR(path_w.as_ptr()), 0x4000_0000)?; // GENERIC_WRITE

            let encoder = self.factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;

            let mut frame: Option<IWICBitmapFrameEncode> = None;
            let mut props: Option<IPropertyBag2> = None;
            encoder.CreateNewFrame(&mut frame, &mut props)?;
            let frame: IWICBitmapFrameEncode =
                frame.ok_or_else(|| Error::Msg("WIC 创建帧失败".into()))?;
            frame.Initialize(None)?;
            frame.SetSize(width as u32, height as u32)?;

            let mut format = GUID_WICPixelFormat32bppBGRA;
            frame.SetPixelFormat(&mut format)?;
            // 像素格式可能被编码器改成别的（PNG 一般是 32bppBGRA，若不支持会转 24bppBGR）
            if format != GUID_WICPixelFormat32bppBGRA {
                // 逐像素降成 BGR 交给编码器，避免颜色错乱
                let mut bgr = Vec::with_capacity(width * height * 3);
                for px in &pixels[..width * height] {
                    bgr.push((*px & 0xFF) as u8);
                    bgr.push(((*px >> 8) & 0xFF) as u8);
                    bgr.push(((*px >> 16) & 0xFF) as u8);
                }
                let stride3 = (width * 3) as u32;
                frame.WritePixels(height as u32, stride3, &bgr)?;
            } else {
                let bytes: &[u8] = std::slice::from_raw_parts(
                    pixels.as_ptr() as *const u8,
                    width * height * 4,
                );
                frame.WritePixels(height as u32, stride, bytes)?;
            }
            frame.Commit()?;
            encoder.Commit()?;
        }
        Ok(())
    }
}

/// 检查 PNG 文件头（测试用）
#[cfg(test)]
pub fn png_signature_ok(path: &Path) -> bool {
    std::fs::read(path)
        .map(|b| b.len() > 8 && b[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_valid_png() {
        let dir = std::env::temp_dir();
        let path = dir.join("scrcpy-rs-shot-test.png");
        let _ = std::fs::remove_file(&path);
        let enc = match WicEncoder::new() {
            Ok(e) => e,
            // 无 COM 环境的 CI 上直接跳过（不算失败）
            Err(_) => return,
        };
        let (w, h) = (32usize, 16usize);
        let pixels = vec![0xFFFF_0000u32; w * h];
        enc.write_png(&path, &pixels, w, h).unwrap();
        assert!(png_signature_ok(&path));
        assert!(std::fs::metadata(&path).unwrap().len() > 50);
        let _ = std::fs::remove_file(&path);
    }
}
