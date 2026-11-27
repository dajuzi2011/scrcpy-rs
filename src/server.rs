//! 设备端 server 的部署、隧道建立与三条 socket 的连接。
//!
//! 与官方 scrcpy v5.0.1 完全一致的流程：
//! 1. 把 server jar 推到 /data/local/tmp/scrcpy-server.jar
//! 2. `adb forward tcp:<port> localabstract:scrcpy_<scid>`
//! 3. `adb shell CLASSPATH=... app_process / com.genymobile.scrcpy.Server <版本> <key=value...>`
//! 4. 按 video → control 顺序连接本地端口（server 端按此顺序 accept）
//! 5. 第一条连接先读 1 字节 dummy，确认隧道真的通了

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant, SystemTime};

use crate::adb::Adb;
use crate::cli::Options;
use crate::error::{Error, Result};
use crate::video::{VideoReader, CODEC_ID_H264};

/// 内嵌官方 server jar（等价于 scrcpy 发行包里的 scrcpy-server-v5.0.1）
static SERVER_JAR: &[u8] = include_bytes!("../tools/scrcpy-server.jar");

/// 必须与 jar 的 BuildConfig.VERSION_NAME 完全一致，否则 server 会拒绝启动
pub const SERVER_VERSION: &str = "5.0.1";

/// `audio_codec=raw` 的 codec id（4 字节 ASCII "raw"）
const CODEC_ID_RAW_AUDIO: u32 = 0x0072_6177;
const DEVICE_SERVER_PATH: &str = "/data/local/tmp/scrcpy-server.jar";

pub struct ServerProc {
    child: Child,
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        // adb shell 是本机进程，杀掉它设备端的 app_process 也会随之结束
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Session {
    pub device_name: String,
    video: Option<VideoReader<TcpStream>>,
    audio: Option<VideoReader<TcpStream>>,
    pub control: Option<TcpStream>,
    pub port: u16,
    pub serial: String,
    _server: ServerProc,
    adb: Adb,
}

impl Session {
    /// 取出视频流读取器（只能取一次）
    pub fn take_video(&mut self) -> Option<VideoReader<TcpStream>> {
        self.video.take()
    }

    /// 取出音频流读取器（只能取一次）
    pub fn take_audio(&mut self) -> Option<VideoReader<TcpStream>> {
        self.audio.take()
    }

    /// 结束时清理 adb forward 规则
    pub fn cleanup(&self) {
        self.adb.forward_remove(&self.serial, self.port);
    }

    /// 复制一份 adb 句柄给后台线程用（只持有可执行文件路径，很轻）
    pub fn adb(&self) -> Adb {
        self.adb.clone()
    }
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

fn random_scid() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 + d.as_secs())
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    ((nanos.wrapping_mul(2_654_435_761) ^ pid.wrapping_mul(40503)) & 0x7FFF_FFFF) as u32
}

/// 把内嵌的 jar 落盘，返回本地路径（只在需要 push 时用到）
fn materialize_jar(version: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("scrcpy-rs-server-{version}.jar"));
    let need_write = match std::fs::metadata(&path) {
        Ok(m) => m.len() != SERVER_JAR.len() as u64,
        Err(_) => true,
    };
    if need_write {
        std::fs::write(&path, SERVER_JAR)?;
    }
    Ok(path)
}

fn ensure_server_pushed(adb: &Adb, serial: &str) -> Result<()> {
    let remote_size = adb.remote_size(serial, DEVICE_SERVER_PATH);
    if remote_size == Some(SERVER_JAR.len() as u64) {
        return Ok(());
    }
    let local = materialize_jar(SERVER_VERSION)?;
    adb.push(serial, &local, DEVICE_SERVER_PATH)?;
    Ok(())
}

fn spawn_log_pipe<R: Read + Send + 'static>(r: R, tag: &'static str) {
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        // 权限提示只打印一次，避免每次触摸都刷屏
        let mut warned_inject = false;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let l = line.trim_end();
                    if !l.is_empty() {
                        println!("[{tag}] {l}");
                    }
                    if !warned_inject && l.contains("INJECT_EVENTS permission") {
                        warned_inject = true;
                        println!(
                            "\n>>> 设备拒绝注入输入事件：请在手机「设置 → 开发者选项」里打开\n\
                             >>> 「USB 调试（安全设置）」/「USB debugging (Security settings)」，\n\
                             >>> 然后重启手机（或重新插拔数据线），触摸和按键才能生效。\n"
                        );
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn connect_once(port: u16) -> Result<TcpStream> {
    let mut sock = TcpStream::connect(("127.0.0.1", port))?;
    sock.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut one = [0u8; 1];
    match sock.read(&mut one) {
        Ok(1) => {}
        Ok(_) => return Err(Error::Msg("隧道已连通但 server 未就绪".into())),
        Err(e) => return Err(Error::Io(e)),
    }
    sock.set_read_timeout(None)?;
    Ok(sock)
}

fn connect_with_retry(port: u16, deadline: Duration) -> Result<TcpStream> {
    let start = Instant::now();
    let mut last_err = Error::Msg("未知错误".into());
    while start.elapsed() < deadline {
        match connect_once(port) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err = e;
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(Error::Msg(format!(
        "连接设备端 server 超时（{:.1}s）：{last_err}",
        deadline.as_secs_f32()
    )))
}

/// 启动 server 并完成三条 socket 的握手
pub fn start(adb: Adb, opts: &Options) -> Result<Session> {
    adb.start_server()?;
    let serial = adb.select_device(opts.serial.as_deref())?;
    println!("[adb] 使用设备: {serial}");

    ensure_server_pushed(&adb, &serial)?;

    let port = free_port()?;
    let scid = random_scid();
    let socket_name = format!("scrcpy_{scid:08x}");
    adb.forward(&serial, port, &socket_name)?;

    // 组装 server 参数：全部是 key=value，且版本号必须是第一个位置参数
    let mut args: Vec<String> = vec![
        format!("CLASSPATH={DEVICE_SERVER_PATH}"),
        "app_process".into(),
        "/".into(),
        "com.genymobile.scrcpy.Server".into(),
        SERVER_VERSION.into(),
        format!("scid={scid:08x}"),
        "log_level=info".into(),
        "tunnel_forward=true".into(),
        "control=true".into(),
    ];
    // 音频：拉裸 PCM，省掉主机端的 Opus 解码
    if opts.audio {
        args.push("audio=true".into());
        args.push("audio_codec=raw".into());
        args.push("audio_source=output".into());
    } else {
        args.push("audio=false".into());
    }
    if opts.max_size > 0 {
        args.push(format!("max_size={}", opts.max_size));
    }
    if opts.bit_rate > 0 {
        args.push(format!("video_bit_rate={}", opts.bit_rate));
    }
    if let Some(fps) = opts.max_fps {
        args.push(format!("max_fps={fps}"));
    }
    if opts.stay_awake {
        args.push("stay_awake=true".into());
    }
    if opts.no_control {
        args.push("control=false".into());
    }

    let mut child = adb.spawn_shell(&serial, &args)?;
    if let Some(out) = child.stdout.take() {
        spawn_log_pipe(out, "device");
    }
    if let Some(err) = child.stderr.take() {
        spawn_log_pipe(err, "device");
    }
    let server = ServerProc { child };

    // 后续任何一步失败，都要把 adb forward 规则撤掉，否则会残留
    match handshake(port, opts.no_control, opts.audio) {
        Ok(h) => Ok(Session {
            device_name: h.device_name,
            video: Some(h.video),
            audio: h.audio,
            control: h.control,
            port,
            serial,
            _server: server,
            adb,
        }),
        Err(e) => {
            adb.forward_remove(&serial, port);
            eprintln!("[adb] 握手失败，已清理端口转发");
            Err(e)
        }
    }
}

struct Handshake {
    device_name: String,
    video: VideoReader<TcpStream>,
    audio: Option<VideoReader<TcpStream>>,
    control: Option<TcpStream>,
}

/// 建立 socket 并完成设备名/编码握手
fn handshake(port: u16, no_control: bool, want_audio: bool) -> Result<Handshake> {
    // 第一条连接（视频）会收到 1 字节 dummy
    let video_sock = connect_with_retry(port, Duration::from_secs(20))?;

    // 第二条连接（音频）。注意 server 端 accept 顺序是 video -> audio -> control，
    // 所以音频必须先连，否则会被当成控制通道
    let audio_sock = if want_audio {
        let s = TcpStream::connect(("127.0.0.1", port))
            .map_err(|e| Error::Msg(format!("音频通道连接失败: {e}")))?;
        Some(s)
    } else {
        None
    };

    // 第三条连接（控制）
    let control_sock = if no_control {
        None
    } else {
        let s = TcpStream::connect(("127.0.0.1", port))
            .map_err(|e| Error::Msg(format!("控制通道连接失败: {e}")))?;
        s.set_nodelay(true)?;
        Some(s)
    };

    let mut video = VideoReader::new(video_sock);
    let device_name = video.read_device_name()?;
    let codec_id = video.read_codec_id()?;
    if codec_id == 0 {
        return Err(Error::Msg("设备端拒绝了视频流（codec id = 0）".into()));
    }
    if codec_id == 1 {
        return Err(Error::Msg(
            "设备端内部错误（codec id = 1），请查看上方 device 日志".into(),
        ));
    }
    if codec_id != CODEC_ID_H264 {
        return Err(Error::Msg(format!(
            "暂不支持该视频编码: {:#010x}（当前只实现 H.264）",
            codec_id
        )));
    }

    // 音频流：只有 4 字节 codec id，没有设备名
    let audio = match audio_sock {
        Some(sock) => {
            let mut audio = VideoReader::new(sock);
            match audio.read_codec_id() {
                Ok(id) if id == CODEC_ID_RAW_AUDIO => {
                    println!("[audio] 已连接（raw PCM）");
                    Some(audio)
                }
                Ok(0) => {
                    println!("[audio] 设备端没有提供音频（可能不支持内录），继续只投画面");
                    None
                }
                Ok(id) => {
                    println!("[audio] 不支持的音频编码 {id:#x}，已丢弃音频流");
                    None
                }
                Err(e) => {
                    eprintln!("[audio] 读取音频头失败: {e}");
                    None
                }
            }
        }
        None => None,
    };

    println!("[adb] 已连接: {device_name}");
    Ok(Handshake {
        device_name,
        video,
        audio,
        control: control_sock,
    })
}
