//! H.264 解码器：Windows Media Foundation（系统内置，零外部依赖、支持硬解）。
//!
//! 做法：解除 MFT 的异步锁，改用同步的 ProcessInput/ProcessOutput 时序；
//! 输入 Annex-B 码流，输出 NV12 系统内存帧。

use std::mem::ManuallyDrop;

use windows::core::{GUID, Interface};
use windows::Win32::Media::MediaFoundation::*;

use crate::error::{Error, Result};
use crate::sps::Dimensions;

/// CLSID_CMSH264DecoderMFT（windows crate 0.58 没有导出，手工定义）
const CLSID_H264_DECODER_MFT: GUID = GUID::from_u128(0x62CE_7E72_4C71_4D20_B15D_4528_31A8_7D9D);

/// 解码出的一帧 NV12 数据。
///
/// 注意三个尺寸/步长的区别（这里踩过坑）：
/// - `width`/`height`：**可见**尺寸，已按 SPS 裁剪，触摸坐标必须用它
/// - `y_stride`：Y 平面每行字节数，通常大于 width（解码器按 16/32/64 对齐）
/// - `uv_offset`：UV 平面起始偏移，等于 `y_stride * 缓冲高度`，**不等于** `y_stride * height`
pub struct RawFrame {
    pub data: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub y_stride: usize,
    pub uv_offset: usize,
    pub uv_stride: usize,
}

pub struct Decoder {
    transform: IMFTransform,
    /// SPS 解析出的真实（裁剪后）尺寸
    sps_dims: Option<Dimensions>,
    /// 输出类型给出的缓冲几何（可能被对齐放大）
    buf_dims: Option<Dimensions>,
    /// 输出类型的 MF_MT_DEFAULT_STRIDE；None 时按缓冲长度反推
    default_stride: Option<usize>,
    /// 可见尺寸 = sps_dims ∩ buf_dims
    visible: Option<Dimensions>,
    /// 输出几何只打印一次，避免刷屏
    logged: bool,
    input_ready: bool,
    output_ready: bool,
    /// 上一次 ProcessInput 之后是否还有输出没抽完
    input_pending: bool,
    /// 解码器是否自己提供输出样本（否则需要我们分配）
    provides_samples: bool,
    /// 我们自己分配输出样本时需要的字节数
    output_buffer_size: u32,
    /// 缓存的 SPS/PPS，关键帧前自动前置
    config: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Result<Self> {
        unsafe {
            let transform: IMFTransform = windows::Win32::System::Com::CoCreateInstance(
                &CLSID_H264_DECODER_MFT,
                None,
                windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
            )
            .map_err(|e| {
                Error::Msg(format!(
                    "无法创建 H.264 解码器 MFT（系统缺少 Media Foundation H.264 解码器？）: {e}"
                ))
            })?;

            // 关闭异步模式，保证 ProcessInput/ProcessOutput 的时序可控
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }

            Ok(Self {
                transform,
                sps_dims: None,
                buf_dims: None,
                default_stride: None,
                visible: None,
                logged: false,
                input_ready: false,
                output_ready: false,
                input_pending: false,
                provides_samples: false,
                output_buffer_size: 0,
                config: Vec::new(),
            })
        }
    }

    /// 记录配置（SPS/PPS），后续关键帧会自动前置
    pub fn set_config(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.config = data.to_vec();
        if let Ok(d) = crate::sps::parse_dimensions(data) {
            if self.sps_dims != Some(d) {
                self.sps_dims = Some(d);
                // 分辨率变了（旋转设备），输入类型要重新协商
                self.input_ready = false;
                self.update_visible();
            }
        }
    }

    /// 当前的 SPS/PPS（Annex-B），录制时作为 MP4 的 sequence header
    pub fn config(&self) -> &[u8] {
        &self.config
    }

    /// 当前可见分辨率（SPS 裁剪后的真实画面尺寸）
    pub fn visible_dims(&self) -> Option<Dimensions> {
        self.visible
    }

    /// 可见尺寸取 SPS 与缓冲几何的交集：SPS 才是设备真正的画面尺寸
    fn update_visible(&mut self) {
        let (sw, sh) = self.sps_dims.map_or((0, 0), |d| (d.width, d.height));
        let (bw, bh) = self.buf_dims.map_or((0, 0), |d| (d.width, d.height));
        let v = match (sw, sh, bw, bh) {
            (0, 0, 0, 0) => None,
            (0, 0, w, h) => Some(Dimensions { width: w, height: h }),
            (w, h, 0, 0) => Some(Dimensions { width: w, height: h }),
            (w, h, bw, bh) => Some(Dimensions {
                width: w.min(bw) & !1,
                height: h.min(bh) & !1,
            }),
        };
        self.visible = v.filter(|d| d.width > 1 && d.height > 1);
    }

    /// 送一个访问单元进去，抽出所有已就绪的输出帧
    pub fn decode(&mut self, data: &[u8], pts_us: u64, key: bool) -> Result<Vec<RawFrame>> {
        let mut out = Vec::new();

        if self.input_pending {
            self.drain(&mut out)?;
        }

        self.ensure_input_type()?;

        // 关键帧前置 SPS/PPS：Android 的 MediaCodec 默认不会把参数集塞进每个 IDR
        let payload = if key && !self.config.is_empty() && !starts_with_parameter_set(data) {
            let mut v = Vec::with_capacity(self.config.len() + data.len());
            v.extend_from_slice(&self.config);
            v.extend_from_slice(data);
            v
        } else {
            data.to_vec()
        };

        unsafe {
            let buffer = MFCreateMemoryBuffer(payload.len() as u32)?;
            {
                let mut ptr: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut ptr, None, None)?;
                std::ptr::copy_nonoverlapping(payload.as_ptr(), ptr, payload.len());
                buffer.Unlock()?;
            }
            buffer.SetCurrentLength(payload.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts_us as i64)?;
            sample.SetSampleDuration(0)?;

            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => self.input_pending = true,
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    // 解码器内部满了：抽干后重试一次
                    self.drain(&mut out)?;
                    self.transform.ProcessInput(0, &sample, 0)?;
                    self.input_pending = true;
                }
                Err(e) => {
                    // 单个包解不了不应该杀进程，丢包并继续（下一个关键帧会恢复）
                    eprintln!("[decode] ProcessInput 失败: {e}");
                    return Ok(out);
                }
            }
        }

        self.drain(&mut out)?;
        Ok(out)
    }

    fn ensure_input_type(&mut self) -> Result<()> {
        if self.input_ready {
            return Ok(());
        }
        unsafe {
            let mt = MFCreateMediaType()?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            mt.SetUINT32(&MF_MT_INTERLACE_MODE, 2)?; // Progressive
            if let Some(d) = self.sps_dims {
                mt.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    ((d.width as u64) << 32) | d.height as u64,
                )?;
            }
            self.transform.SetInputType(0, &mt, 0)?;
            self.input_ready = true;
        }
        Ok(())
    }

    /// 抽取所有可用输出帧，直到 NEED_MORE_INPUT
    fn drain(&mut self, out: &mut Vec<RawFrame>) -> Result<()> {
        loop {
            if !self.output_ready {
                if let Err(e) = self.negotiate_output() {
                    eprintln!("[decode] 输出类型协商失败: {e}");
                    self.input_pending = false;
                    return Ok(());
                }
            }

            let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default()];
            // 大多数解码 MFT 不提供输出样本，必须由调用方准备一个带缓冲的 IMFSample
            let our_sample = if self.provides_samples {
                None
            } else {
                match unsafe {
                    let sample = MFCreateSample()?;
                    let buf = MFCreateMemoryBuffer(self.output_buffer_size)?;
                    sample.AddBuffer(&buf)?;
                    Ok::<_, Error>(sample)
                } {
                    Ok(s) => Some(s),
                    Err(e) => {
                        eprintln!("[decode] 分配输出缓冲失败: {e}");
                        self.input_pending = false;
                        return Ok(());
                    }
                }
            };
            if let Some(s) = &our_sample {
                buffers[0].pSample = ManuallyDrop::new(Some(s.clone()));
            }
            let mut status = 0u32;
            let r = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };

            match r {
                Ok(()) => {
                    self.input_pending = false;
                    unsafe {
                        let sample = ManuallyDrop::take(&mut buffers[0].pSample);
                        let _events = ManuallyDrop::take(&mut buffers[0].pEvents);
                        if let Some(sample) = sample {
                            if let Some(frame) = self.extract(&sample)? {
                                out.push(frame);
                            }
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    self.input_pending = false;
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // 分辨率变了：重新协商输出类型后继续
                    self.output_ready = false;
                    continue;
                }
                Err(e) => {
                    eprintln!("[decode] ProcessOutput 失败: {e}");
                    self.input_pending = false;
                    return Ok(());
                }
            }
        }
    }

    fn negotiate_output(&mut self) -> Result<()> {
        unsafe {
            let mut chosen: Option<IMFMediaType> = None;
            let mut dims = self.sps_dims;

            // 优先选解码器自己推荐的 NV12
            let mut idx = 0u32;
            while idx < 24 {
                let mt = match self.transform.GetOutputAvailableType(0, idx) {
                    Ok(mt) => mt,
                    Err(_) => break,
                };
                idx += 1;
                if let Ok(sub) = mt.GetGUID(&MF_MT_SUBTYPE) {
                    if sub == MFVideoFormat_NV12 {
                        if let Ok(fs) = mt.GetUINT64(&MF_MT_FRAME_SIZE) {
                            let w = (fs >> 32) as u32;
                            let h = (fs & 0xFFFF_FFFF) as u32;
                            if w > 0 && h > 0 {
                                dims = Some(Dimensions {
                                    width: w,
                                    height: h,
                                });
                            }
                        }
                        chosen = Some(mt);
                        break;
                    }
                }
            }

            let mt = match chosen {
                Some(mt) => mt,
                None => {
                    let d = dims.ok_or_else(|| crate::errmsg!("解码器没有可用输出类型且尺寸未知"))?;
                    let mt = MFCreateMediaType()?;
                    mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                    mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                    mt.SetUINT64(
                        &MF_MT_FRAME_SIZE,
                        ((d.width as u64) << 32) | d.height as u64,
                    )?;
                    mt.SetUINT64(&MF_MT_FRAME_RATE, (30u64 << 32) | 1)?;
                    mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
                    mt.SetUINT32(&MF_MT_INTERLACE_MODE, 2)?;
                    mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
                    mt
                }
            };

            self.transform.SetOutputType(0, &mt, 0)?;
            let fs = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            let buf_w = (fs >> 32) as u32;
            let buf_h = (fs & 0xFFFF_FFFF) as u32;
            if buf_w == 0 || buf_h == 0 {
                return Err(crate::errmsg!("解码器输出尺寸为 0"));
            }
            self.buf_dims = Some(Dimensions {
                width: buf_w,
                height: buf_h,
            });

            // MF_MT_DEFAULT_STRIDE 是权威的行跨度（字节）；有些解码器不设这个属性，
            // 那时只能按缓冲长度反推（见 infer_stride）
            self.default_stride = mt
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .ok()
                .map(|v| (v as i32).unsigned_abs() as usize)
                .filter(|s| *s >= buf_w as usize);

            let info = self.transform.GetOutputStreamInfo(0)?;
            self.provides_samples =
                info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            // 自己分配时多留一些余量（有些解码器会给对齐后的更大缓冲）
            let stride = self.default_stride.unwrap_or_else(|| align_up(buf_w as usize, 64));
            let needed = stride * buf_h as usize * 3 / 2;
            self.output_buffer_size = info
                .cbSize
                .max(needed as u32)
                .max(buf_w * buf_h * 3 / 2);

            self.update_visible();
            if self.visible.is_none() {
                return Err(crate::errmsg!("无法确定可见分辨率"));
            }

            self.output_ready = true;
            Ok(())
        }
    }

    /// 反推 Y 平面行跨度
    fn infer_stride(&self, len: usize) -> usize {
        let (bw, bh) = match self.buf_dims {
            Some(d) => (d.width as usize, d.height as usize),
            None => return 0,
        };
        if let Some(s) = self.default_stride {
            return s;
        }
        // 最理想：缓冲正好是 stride * h * 3/2
        let denom = bh * 3 / 2;
        if denom > 0 && len % denom == 0 {
            let s = len / denom;
            if s >= bw {
                return s;
            }
        }
        // 否则按对齐宽度猜一个，取至少能装下 bw 的最小对齐值
        let s = align_up(bw, 16);
        if len >= s * denom {
            return s;
        }
        // 最后退路：按长度估算并向上取偶
        let s = len * 2 / (3 * bh.max(1));
        (s.max(bw) + 1) & !1
    }

    fn extract(&mut self, sample: &IMFSample) -> Result<Option<RawFrame>> {
        let dims = match self.visible {
            Some(d) => d,
            None => return Ok(None),
        };
        let (buf_w, buf_h) = match self.buf_dims {
            Some(d) => (d.width as usize, d.height as usize),
            None => return Ok(None),
        };
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len = 0u32;
            let mut cur_len = 0u32;
            buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
            let len = cur_len as usize;
            if len == 0 {
                buffer.Unlock()?;
                return Ok(None);
            }
            let w = dims.width as usize;
            let h = dims.height as usize;

            let y_stride = self.infer_stride(len);
            if y_stride < buf_w {
                buffer.Unlock()?;
                return Err(crate::errmsg!(
                    "推断出的行跨度过小: stride={y_stride} < {buf_w}"
                ));
            }

            // NV12 的 UV 平面紧跟在 Y 平面之后；Y 平面用的是**缓冲高度**，
            // 不是可见高度。用总长度兜底，兼容尾部有多余填充的解码器。
            let uv_rows = buf_h.div_ceil(2);
            let mut uv_offset = y_stride * buf_h;
            if uv_offset + y_stride * uv_rows > len {
                // 尝试「没有高度对齐」的排布
                let alt = y_stride * h;
                if alt + y_stride * (h.div_ceil(2)) <= len {
                    uv_offset = alt;
                } else if len > y_stride * uv_rows {
                    // 尾部有填充：UV 在缓冲区末尾
                    uv_offset = len - y_stride * uv_rows;
                }
            }
            let need = uv_offset + y_stride * h.div_ceil(2);
            if len < need {
                buffer.Unlock()?;
                return Err(crate::errmsg!(
                    "解码输出缓冲区过小: {len} < {need}（缓冲 {buf_w}x{buf_h}, 可见 {w}x{h}, stride {y_stride}）"
                ));
            }

            // 只需要拷贝可见区域用得到的部分（Y 的 h 行 + UV 的 h/2 行）
            let uv_chroma_rows = h.div_ceil(2);
            let copy_len = uv_offset + y_stride * uv_chroma_rows;
            let mut data = vec![0u8; copy_len];
            if copy_len == len && uv_offset == y_stride * buf_h {
                std::ptr::copy_nonoverlapping(ptr, data.as_mut_ptr(), copy_len);
            } else {
                // 分两段拷贝，跳过中间的填充行
                std::ptr::copy_nonoverlapping(ptr, data.as_mut_ptr(), y_stride * h);
                std::ptr::copy_nonoverlapping(
                    ptr.add(uv_offset),
                    data.as_mut_ptr().add(uv_offset),
                    y_stride * uv_chroma_rows,
                );
            }
            buffer.Unlock()?;

            if !self.logged {
                self.logged = true;
                println!(
                    "[decode] NV12 缓冲 {buf_w}x{buf_h} 可见 {w}x{h} stride={y_stride} uv_offset={uv_offset} len={len}"
                );
            }

            Ok(Some(RawFrame {
                data,
                width: w,
                height: h,
                y_stride,
                uv_offset,
                uv_stride: y_stride,
            }))
        }
    }
}

/// 数据里是否已经自带 SPS（NAL type 7）
fn starts_with_parameter_set(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let t = data[i + 3] & 0x1F;
            return t == 7 || t == 9;
        }
        i += 1;
    }
    false
}

/// 程序启动时调用一次
pub fn startup() -> Result<()> {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;
    }
    Ok(())
}

pub fn shutdown() {
    unsafe {
        let _ = MFShutdown();
    }
}

/// 把 RawFrame 的 NV12 拆成 (y, uv) 两个平面（已按 uv_offset 对齐真正的平面起点）
pub fn planes(f: &RawFrame) -> (&[u8], &[u8]) {
    let y_len = f.uv_offset.min(f.data.len());
    (&f.data[..y_len], &f.data[y_len..])
}

#[inline]
fn align_up(v: usize, align: usize) -> usize {
    if align == 0 {
        return v;
    }
    (v + align - 1) / align * align
}

// 保证 Interface 导入被使用（IMFMediaType 的方法依赖它）
const _: fn() = || {
    fn _assert<T: Interface>() {}
    _assert::<IMFMediaType>();
};

#[cfg(test)]
mod tests {
    use super::*;

    /// 设备上真实遇到的排布：可见 1200x2670，解码器缓冲高度补齐到 2672，
    /// UV 平面从 stride*buf_h 开始。planes() 必须按 uv_offset 切，而不是按可见高度。
    #[test]
    fn planes_follow_uv_offset() {
        let (w, h, buf_h) = (1200usize, 2670usize, 2672usize);
        let stride = w;
        let uv_offset = stride * buf_h;
        let data = vec![0u8; uv_offset + stride * (h / 2)];
        let f = RawFrame {
            data,
            width: w,
            height: h,
            y_stride: stride,
            uv_offset,
            uv_stride: stride,
        };
        let (y, uv) = planes(&f);
        assert_eq!(y.len(), uv_offset);
        assert_eq!(uv.len(), stride * (h / 2));
        // 如果按 y_stride * height 切，会少掉整整两行色度
        assert_ne!(y.len(), stride * h);
    }
}
