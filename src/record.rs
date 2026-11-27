//! 录屏：把视频流里的 H.264 访问单元直接封装进 MP4。
//!
//! 关键点：**不重新编码**。scrcpy 已经在设备端编好了 H.264，主机端只需要把
//! 收到的访问单元按时间戳喂给 Media Foundation 的 SinkWriter，由它负责 MP4 封装。
//! 如果 SinkWriter 起不来（老系统 / 缺组件），退化成写 Annex-B 裸流（.h264），
//! 至少数据是完整的，能用 ffmpeg/VLC 打开。

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use windows::core::PCWSTR;
use windows::Win32::Media::MediaFoundation::*;

use crate::error::{Error, Result};

enum Backend {
    Mp4 { writer: IMFSinkWriter, stream: u32 },
    Raw { file: File },
}

#[allow(dead_code)]
pub struct Recorder {
    backend: Backend,
    pub path: PathBuf,
    started: Instant,
    pub samples: u64,
    /// 是否为 MP4（否则是裸流）
    pub mp4: bool,
    /// 首个/最后一个包的时间戳（微秒），用来还原真实帧率
    first_pts: Option<u64>,
    last_pts: u64,
}

/// 把 Annex-B 的 SPS/PPS 转成 AVCDecoderConfigurationRecord（avcC）。
/// Media Foundation 的 MF_MT_MPEG_SEQUENCE_HEADER 要的是 avcC，不是裸 Annex-B，
/// 直接塞 Annex-B 会被 SinkWriter 以 MF_E_INVALIDMEDIATYPE 拒绝。
pub fn avcc_from_annexb(cfg: &[u8]) -> Option<Vec<u8>> {
    let mut sps: Option<&[u8]> = None;
    let mut pps: Option<&[u8]> = None;
    for (t, payload) in crate::sps::annexb_units(cfg) {
        // annexb_units 已经把 NAL 头剥掉了
        if t == 7 && sps.is_none() {
            sps = Some(payload);
        } else if t == 8 && pps.is_none() {
            pps = Some(payload);
        }
    }
    let (sps, pps) = (sps?, pps?);
    if sps.len() < 4 || sps.len() > 0xFFFF || pps.len() > 0xFFFF {
        return None;
    }
    let mut out = Vec::with_capacity(11 + sps.len() + pps.len());
    out.push(1); // configurationVersion
    out.push(sps[1]); // AVCProfileIndication
    out.push(sps[2]); // profile_compatibility
    out.push(sps[3]); // AVCLevelIndication
    out.push(0xFF); // 6 bits reserved + lengthSizeMinusOne = 3
    out.push(0xE1); // 3 bits reserved + numOfSPS = 1
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1); // numOfPPS
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    Some(out)
}

/// 生成带时间戳的文件名，放在 `dir` 下
pub fn default_path(dir: &Path, prefix: &str, ext: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = time_parts();
    let _ = stamp;
    dir.join(format!("{prefix}-{t}.{ext}"))
}

fn time_parts() -> String {
    // 用本地时间做文件名（不引入 chrono，直接用系统时间接口）
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

impl Recorder {
    /// 开始录制。`fps` 用于给 SinkWriter 声明帧率（不影响实际时间戳）
    pub fn start(path: PathBuf, width: u32, height: u32, fps: u32, seq_header: &[u8]) -> Result<Self> {
        match Self::start_mp4(&path, width, height, fps, seq_header) {
            Ok(r) => Ok(r),
            Err(e) => {
                eprintln!("[rec] MP4 封装不可用（{e}），先录裸流 .h264，结束后再用 ffmpeg 转封装");
                // SinkWriter 可能已经建了一个空文件，别留垃圾
                let _ = std::fs::remove_file(&path);
                let raw_path = path.with_extension("h264");
                let mut file = File::create(&raw_path)?;
                // 裸流没有容器，必须自己把 SPS/PPS 写在最前面，
                // 否则播放器和 ffmpeg 都不知道分辨率/编码参数
                if !seq_header.is_empty() {
                    let _ = file.write_all(seq_header);
                    let _ = file.flush();
                }
                Ok(Self {
                    backend: Backend::Raw { file },
                    path: raw_path,
                    started: Instant::now(),
                    samples: 0,
                    mp4: false,
                    first_pts: None,
                    last_pts: 0,
                })
            }
        }
    }

    fn start_mp4(path: &Path, width: u32, height: u32, fps: u32, seq_header: &[u8]) -> Result<Self> {
        unsafe {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 1)?;
            let attrs = attrs.ok_or_else(|| Error::Msg("MFCreateAttributes 失败".into()))?;
            // 关掉限速：我们是实时喂数据，限速只会让 WriteSample 阻塞
            let _ = attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1);

            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let writer: IMFSinkWriter =
                MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, &attrs)?;

            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, 16_000_000)?;
            out.SetUINT64(
                &MF_MT_FRAME_SIZE,
                ((width as u64) << 32) | height as u64,
            )?;
            out.SetUINT64(&MF_MT_FRAME_RATE, ((fps.max(1) as u64) << 32) | 1)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, 2)?; // Progressive
            let stream = writer.AddStream(&out)?;

            // 输入就是压缩后的 H.264，SinkWriter 只做封装
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, ((fps.max(1) as u64) << 32) | 1)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, 2)?;
            // SPS/PPS：有了它 MP4 的 avcC 才完整，从中途开始录也能正常解码
            if let Some(avcc) = avcc_from_annexb(seq_header) {
                input.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &avcc)?;
            }
            writer.SetInputMediaType(stream, &input, None)?;
            writer.BeginWriting()?;

            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            Ok(Self {
                backend: Backend::Mp4 { writer, stream },
                path: path.to_path_buf(),
                started: Instant::now(),
                samples: 0,
                mp4: true,
                first_pts: None,
                last_pts: 0,
            })
        }
    }

    /// 写入一个访问单元。`pts_us` 是设备端时间戳（微秒）
    pub fn write(&mut self, data: &[u8], pts_us: u64, _key: bool) -> Result<()> {
        self.samples += 1;
        if self.first_pts.is_none() {
            self.first_pts = Some(pts_us);
        }
        self.last_pts = self.last_pts.max(pts_us);
        match &mut self.backend {
            Backend::Mp4 { writer, stream } => unsafe {
                let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
                {
                    let mut ptr: *mut u8 = std::ptr::null_mut();
                    buffer.Lock(&mut ptr, None, None)?;
                    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
                    buffer.Unlock()?;
                }
                buffer.SetCurrentLength(data.len() as u32)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                // MF 用 100ns 为单位
                sample.SetSampleTime((pts_us * 10) as i64)?;
                sample.SetSampleDuration(0)?;
                writer.WriteSample(*stream, &sample)?;
                Ok(())
            },
            Backend::Raw { file } => {
                file.write_all(data)?;
                Ok(())
            }
        }
    }

    /// 结束录制，返回文件路径
    pub fn finish(mut self) -> Result<(PathBuf, u64)> {
        let path = self.path.clone();
        let samples = self.samples;
        let is_raw = matches!(self.backend, Backend::Raw { .. });
        match &mut self.backend {
            Backend::Mp4 { writer, .. } => unsafe {
                if let Err(e) = writer.Finalize() {
                    eprintln!("[rec] Finalize 失败: {e}");
                }
            },
            Backend::Raw { file } => {
                let _ = file.flush();
            }
        }
        let mut final_path = path;
        if is_raw {
            // 用真实时间戳还原平均帧率，否则裸流会被当成固定帧率，播放速度不对
            let fps = match self.first_pts {
                Some(first) if self.last_pts > first && self.samples > 1 => {
                    let secs = (self.last_pts - first) as f64 / 1_000_000.0;
                    ((self.samples - 1) as f64 / secs).clamp(1.0, 240.0)
                }
                _ => 60.0,
            };
            if let Some(mp4) = remux_with_ffmpeg(&final_path, fps) {
                final_path = mp4;
            }
        }
        Ok((final_path, samples))
    }

    #[allow(dead_code)]
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }
}

/// 裸流 .h264 不是所有播放器都认，若系统里有 ffmpeg 就顺手转成 mp4（无损，只是换容器）
fn remux_with_ffmpeg(raw: &Path, fps: f64) -> Option<std::path::PathBuf> {
    let size = std::fs::metadata(raw).map(|m| m.len()).unwrap_or(0);
    if size < 16 * 1024 {
        return None; // 太短，没必要
    }
    let Some(ffmpeg) = find_ffmpeg() else {
        println!(
            "[rec] 未找到 ffmpeg，保留裸流：{}（可用 VLC 播放）",
            raw.display()
        );
        return None;
    };
    let mp4 = raw.with_extension("mp4");
    let rate = format!("{:.3}", fps);
    let status = std::process::Command::new(ffmpeg)
        .args(["-y", "-loglevel", "error", "-r", &rate, "-i"])
        .arg(raw)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(&mp4)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags_no_window()
        .status();
    match status {
        Ok(s) if s.success() && std::fs::metadata(&mp4).map(|m| m.len()).unwrap_or(0) > 4096 => {
            let _ = std::fs::remove_file(raw);
            println!("[rec] 已转封装为 {}", mp4.display());
            Some(mp4)
        }
        _ => {
            println!("[rec] 转封装失败，保留裸流：{}", raw.display());
            None
        }
    }
}

fn find_ffmpeg() -> Option<std::path::PathBuf> {
    // 先看 PATH
    let probe = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags_no_window()
        .status();
    if matches!(probe, Ok(s) if s.success()) {
        return Some(std::path::PathBuf::from("ffmpeg"));
    }
    None
}

/// Windows 下不要弹黑窗
trait NoWindow {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

impl NoWindow for std::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            self.creation_flags(0x0800_0000);
        }
        self
    }
}

// SinkWriter 是自由线程的 COM 对象，且我们全程用 Mutex 串行访问，
// 这里显式声明可跨线程传递（Rust 的 COM 包装默认不实现 Send）
unsafe impl Send for Recorder {}

// encode_wide 需要 OsStrExt
trait OsStrExt {
    fn encode_wide(&self) -> std::vec::IntoIter<u16>;
}

impl OsStrExt for std::ffi::OsStr {
    fn encode_wide(&self) -> std::vec::IntoIter<u16> {
        let v: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(self).collect();
        v.into_iter()
    }
}
