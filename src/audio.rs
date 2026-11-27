//! 音频播放：设备端发来的是 **裸 PCM**（`audio_codec=raw`，48kHz / 立体声 / S16LE），
//! 所以不需要 Opus 解码器，直接用老牌的 `waveOut` 送到默认输出设备。
//!
//! 为什么用 waveOut 而不是 WASAPI：WASAPI 要自己管 IAudioClient / 混音格式 / 事件驱动，
//! 而这里的数据本来就是 48k 立体声——正是声卡混音器最常见的格式，交给系统重采样即可。
//! 代价是延迟略高（100ms 量级），对投屏够用。
//!
//! 线程模型：网络线程 `push()` 原始字节进环形缓冲，播放线程按 20ms 一块往外送，
//! 欠载时补静音，避免卡顿/爆音。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::Win32::Media::Audio::{
    waveOutClose, waveOutOpen, waveOutPrepareHeader, waveOutReset, waveOutUnprepareHeader,
    waveOutWrite, CALLBACK_NULL, WAVEFORMATEX, WAVEHDR, WAVE_MAPPER, WAVE_FORMAT_PCM,
};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const BITS: u16 = 16;
/// 每块 20ms
const FRAMES_PER_BUF: usize = 960;
const SAMPLES_PER_BUF: usize = FRAMES_PER_BUF * CHANNELS as usize;
const BUF_COUNT: usize = 4;
/// 环形缓冲上限（约 400ms），超过就丢最老的，控制延迟
const MAX_SAMPLES: usize = SAMPLE_RATE as usize * CHANNELS as usize * 4 / 10;

pub struct AudioPlayer {
    ring: Arc<Mutex<VecDeque<i16>>>,
    on: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// 设备是否成功打开（失败时面板要能显示）
    ok: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    pub fn start() -> Self {
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_SAMPLES)));
        let on = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let ok = Arc::new(AtomicBool::new(false));

        let thread = {
            let ring = ring.clone();
            let on = on.clone();
            let stop = stop.clone();
            let ok = ok.clone();
            std::thread::spawn(move || playback_loop(ring, on, stop, ok))
        };

        Self {
            ring,
            on,
            stop,
            ok,
            thread: Some(thread),
        }
    }

    /// 推入裸 PCM 字节（S16LE 交错）
    pub fn push(&self, data: &[u8]) {
        if data.len() < 2 {
            return;
        }
        let mut ring = match self.ring.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let samples = data.len() / 2;
        // 缓冲积压说明播放端跟不上，丢最老的保持低延迟
        while ring.len() + samples > MAX_SAMPLES {
            if ring.pop_front().is_none() {
                break;
            }
        }
        for i in 0..samples {
            let s = i16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
            ring.push_back(s);
        }
    }

    pub fn set_on(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        if !on {
            // 关掉时清空积压，重新打开不会有旧声音
            if let Ok(mut g) = self.ring.lock() {
                g.clear();
            }
        }
    }

    #[allow(dead_code)]
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    pub fn is_ok(&self) -> bool {
        self.ok.load(Ordering::Relaxed)
    }

    /// 缓冲里有多少毫秒的数据（调试用）
    #[allow(dead_code)]
    pub fn buffered_ms(&self) -> u32 {
        self.ring
            .lock()
            .map(|g| (g.len() / CHANNELS as usize * 1000 / SAMPLE_RATE as usize) as u32)
            .unwrap_or(0)
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn playback_loop(
    ring: Arc<Mutex<VecDeque<i16>>>,
    on: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    ok: Arc<AtomicBool>,
) {
    let mut handle = windows::Win32::Media::Audio::HWAVEOUT::default();
    let fmt = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * CHANNELS as u32 * (BITS as u32 / 8),
        nBlockAlign: CHANNELS * (BITS / 8),
        wBitsPerSample: BITS,
        cbSize: 0,
    };
    let rc = unsafe {
        waveOutOpen(
            Some(&mut handle),
            WAVE_MAPPER,
            &fmt,
            0,
            0,
            CALLBACK_NULL,
        )
    };
    if rc != 0 {
        eprintln!("[audio] 打开音频输出失败（waveOutOpen = {rc}），音频转发不可用");
        ok.store(false, Ordering::Relaxed);
        // 不开设备也要留一个空转线程：避免网络线程写满缓冲
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if let Ok(mut g) = ring.lock() {
                g.clear();
            }
        }
        return;
    }
    ok.store(true, Ordering::Relaxed);
    eprintln!("[audio] 音频输出已就绪（48kHz 立体声 16bit）");

    // 每块缓冲单独 Box，保证交给驱动期间地址稳定
    struct Buf {
        data: Vec<i16>,
        hdr: WAVEHDR,
    }
    let mut bufs: Vec<Box<Buf>> = (0..BUF_COUNT)
        .map(|_| {
            Box::new(Buf {
                data: vec![0i16; SAMPLES_PER_BUF],
                hdr: WAVEHDR::default(),
            })
        })
        .collect();

    for b in bufs.iter_mut() {
        let b = b.as_mut();
        b.hdr.lpData = windows::core::PSTR(b.data.as_mut_ptr() as *mut u8);
        b.hdr.dwBufferLength = (SAMPLES_PER_BUF * 2) as u32;
        unsafe {
            waveOutPrepareHeader(handle, &mut b.hdr, std::mem::size_of::<WAVEHDR>() as u32);
            waveOutWrite(handle, &mut b.hdr, std::mem::size_of::<WAVEHDR>() as u32);
        }
    }

    const WHDR_DONE: u32 = 0x0000_0001;
    while !stop.load(Ordering::Relaxed) {
        let play = on.load(Ordering::Relaxed);
        for b in bufs.iter_mut() {
            let b = b.as_mut();
            if b.hdr.dwFlags & WHDR_DONE == 0 {
                continue;
            }
            // 取数据（关闭或欠载都填静音）
            if play {
                if let Ok(mut g) = ring.lock() {
                    for slot in b.data.iter_mut() {
                        *slot = g.pop_front().unwrap_or(0);
                    }
                }
            } else {
                b.data.iter_mut().for_each(|s| *s = 0);
            }
            b.hdr.dwFlags &= !WHDR_DONE;
            unsafe {
                waveOutWrite(handle, &mut b.hdr, std::mem::size_of::<WAVEHDR>() as u32);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(6));
    }

    unsafe {
        waveOutReset(handle);
        for b in bufs.iter_mut() {
            let b = b.as_mut();
            waveOutUnprepareHeader(handle, &mut b.hdr, std::mem::size_of::<WAVEHDR>() as u32);
        }
        waveOutClose(handle);
    }
}
