//! 视频流解复用：完全按 scrcpy v5.0.1 的线上格式实现。
//!
//! 线上格式（`send_device_meta`/`send_stream_meta`/`send_frame_meta` 全为默认 true）：
//!
//! ```text
//! [64 字节设备名，UTF-8，不足补 0]
//! [4 字节 codec id，大端]
//! 循环:
//!   [12 字节包头]
//!     若首字节最高位为 1 -> session 包，包头即全部内容：flags(4) width(4) height(4)
//!     否则 -> 媒体包：pts|flags(8) size(4)，紧跟 size 字节数据
//! ```
//!
//! pts 的高位被借用来做标志：bit62 = config 包（SPS/PPS），bit61 = 关键帧。

use std::io::Read;

use crate::error::{Error, Result};

pub const DEVICE_NAME_FIELD_LENGTH: usize = 64;
pub const PACKET_HEADER_SIZE: usize = 12;

const PACKET_FLAG_SESSION: u64 = 1 << 63;
const PACKET_FLAG_CONFIG: u64 = 1 << 62;
const PACKET_FLAG_KEY_FRAME: u64 = 1 << 61;
const PACKET_PTS_MASK: u64 = PACKET_FLAG_KEY_FRAME - 1;

/// H.264 的 codec id（4 字节 ASCII 'h264'）
pub const CODEC_ID_H264: u32 = 0x6832_3634;

#[derive(Debug)]
pub enum Packet {
    /// 会话元数据：当前画面尺寸
    Session { width: u32, height: u32 },
    /// 编码器配置（SPS/PPS），不是可显示帧
    Config(Vec<u8>),
    /// 一个访问单元
    Frame {
        pts: u64,
        key: bool,
        data: Vec<u8>,
    },
}

pub struct VideoReader<R: Read> {
    inner: R,
    header: [u8; PACKET_HEADER_SIZE],
    payload: Vec<u8>,
}

impl<R: Read> VideoReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            header: [0u8; PACKET_HEADER_SIZE],
            payload: Vec::new(),
        }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inner.read_exact(buf).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::Msg("设备端已断开连接（视频流结束）".into())
            } else {
                Error::Io(e)
            }
        })
    }

    /// 读取首部 64 字节设备名
    pub fn read_device_name(&mut self) -> Result<String> {
        let mut buf = [0u8; DEVICE_NAME_FIELD_LENGTH];
        self.read_exact(&mut buf)?;
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Ok(String::from_utf8_lossy(&buf[..end]).trim().to_string())
    }

    /// 读取 4 字节 codec id
    pub fn read_codec_id(&mut self) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)?;
        Ok(u32::from_be_bytes(buf))
    }

    pub fn read_packet(&mut self) -> Result<Packet> {
        let mut header = self.header;
        self.read_exact(&mut header)?;
        self.header = header;

        if header[0] & 0x80 != 0 {
            // session 包
            let width = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
            let height = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
            // header[3] & 1 是客户端发起的 resize 标志，本实现不区分
            return Ok(Packet::Session { width, height });
        }

        let pts_flags = u64::from_be_bytes([
            header[0], header[1], header[2], header[3], header[4], header[5], header[6], header[7],
        ]);
        let size = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
        if size == 0 {
            return Err(Error::Msg("收到长度为 0 的视频包".into()));
        }

        self.payload.clear();
        self.payload.resize(size, 0);
        // 借用拆分：payload 单独取出，避免与 self 的其它字段冲突
        let mut payload = std::mem::take(&mut self.payload);
        let r = self.inner.read_exact(&mut payload);
        self.payload = payload;
        r.map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::Msg("设备端已断开连接（视频流结束）".into())
            } else {
                Error::Io(e)
            }
        })?;

        if pts_flags & PACKET_FLAG_SESSION != 0 {
            // 理论上不会走到这里（session 包首字节一定有 0x80），保险起见按 session 处理
            return Ok(Packet::Session { width: 0, height: 0 });
        }
        if pts_flags & PACKET_FLAG_CONFIG != 0 {
            return Ok(Packet::Config(self.payload.clone()));
        }
        Ok(Packet::Frame {
            pts: pts_flags & PACKET_PTS_MASK,
            key: pts_flags & PACKET_FLAG_KEY_FRAME != 0,
            data: self.payload.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    /// 起一个假的 scrcpy server，按 v5.0.1 的字节序发数据，
    /// 校验客户端的设备名/编码/包头解析是否完全对齐。
    #[test]
    fn handshake_and_packets() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let payload_cfg: Vec<u8> = vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, 0xAA];
        let payload_key: Vec<u8> = vec![0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x21];
        let cfg2 = payload_cfg.clone();
        let key2 = payload_key.clone();

        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // 1) dummy byte：只有第一条连接会收到
            s.write_all(&[0xA5]).unwrap();
            // 2) 64 字节设备名
            let mut name = [0u8; 64];
            name[..9].copy_from_slice(b"Pixel-123");
            s.write_all(&name).unwrap();
            // 3) 4 字节 codec id
            s.write_all(&CODEC_ID_H264.to_be_bytes()).unwrap();
            // 4) session 包：最高位为 1，后面跟宽高
            let mut hdr = [0u8; 12];
            hdr[0] = 0x80;
            hdr[4..8].copy_from_slice(&1080u32.to_be_bytes());
            hdr[8..12].copy_from_slice(&2400u32.to_be_bytes());
            s.write_all(&hdr).unwrap();
            // 5) config 包（pts 的 bit62 置位）
            let mut hdr = [0u8; 12];
            let flags = 1u64 << 62;
            hdr[..8].copy_from_slice(&flags.to_be_bytes());
            hdr[8..].copy_from_slice(&(cfg2.len() as u32).to_be_bytes());
            s.write_all(&hdr).unwrap();
            s.write_all(&cfg2).unwrap();
            // 6) 关键帧（bit61 置位，pts = 123456）
            let mut hdr = [0u8; 12];
            let flags = (1u64 << 61) | 123_456;
            hdr[..8].copy_from_slice(&flags.to_be_bytes());
            hdr[8..].copy_from_slice(&(key2.len() as u32).to_be_bytes());
            s.write_all(&hdr).unwrap();
            s.write_all(&key2).unwrap();
        });

        // 客户端
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut one = [0u8; 1];
        let mut r = sock.try_clone().unwrap();
        use std::io::Read;
        r.read_exact(&mut one).unwrap();
        assert_eq!(one[0], 0xA5, "第一条连接应先收到 dummy 字节");

        let mut vr = VideoReader::new(sock);
        assert_eq!(vr.read_device_name().unwrap(), "Pixel-123");
        assert_eq!(vr.read_codec_id().unwrap(), CODEC_ID_H264);

        match vr.read_packet().unwrap() {
            Packet::Session { width, height } => {
                assert_eq!((width, height), (1080, 2400));
            }
            other => panic!("应收到 session 包, 实际: {other:?}"),
        }

        match vr.read_packet().unwrap() {
            Packet::Config(data) => assert_eq!(data, payload_cfg),
            other => panic!("应收到 config 包, 实际: {other:?}"),
        }

        match vr.read_packet().unwrap() {
            Packet::Frame { pts, key, data } => {
                assert_eq!(pts, 123_456);
                assert!(key);
                assert_eq!(data, payload_key);
            }
            other => panic!("应收到帧, 实际: {other:?}"),
        }

        server.join().unwrap();
    }

    #[test]
    fn zero_length_packet_is_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let hdr = [0u8; 12]; // 全 0：size = 0
            s.write_all(&hdr).unwrap();
        });
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut vr = VideoReader::new(sock);
        assert!(vr.read_packet().is_err());
    }
}
