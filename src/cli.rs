//! 命令行参数解析（手写，零依赖）。

use std::path::PathBuf;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Options {
    /// 目标设备序列号；None 表示自动选择唯一在线设备
    pub serial: Option<String>,
    /// 视频最长边像素；0 表示不限制（原分辨率）
    pub max_size: u32,
    /// 视频码率，bit/s
    pub bit_rate: u32,
    /// 最大采集帧率；None 表示不限制。
    /// 默认 60：不限制时 120Hz 的手机会按屏幕刷新率推流，而本机解码 + NV12→BGRA
    /// 在 1200x2670 上只跑得到 ~98fps，差出来的帧会堆在 socket 里，延迟一路涨到秒级。
    pub max_fps: Option<f32>,
    /// 保持设备不休眠
    pub stay_awake: bool,
    /// 启动后关闭设备屏幕（仅投屏，触摸仍可用）
    pub turn_screen_off: bool,
    /// 关闭输入控制（只看不摸）
    pub no_control: bool,
    /// 只列出设备后退出
    pub list_devices: bool,
    /// 自定义标题
    pub window_title: Option<String>,
    /// 启动即全屏
    pub fullscreen: bool,
    /// 指定 adb 可执行文件路径
    pub adb_path: Option<PathBuf>,
    /// 调试模式：直接播放本地 Annex-B 裸流文件（H.264/H.265），不连接设备
    pub play_file: Option<PathBuf>,
    /// 调试：把第 20 帧解码结果导出成 BMP，用于验证渲染链路
    pub dump_frame: Option<PathBuf>,
    /// 启动即打开「界面层级叠加」（uiautomator）
    pub ui_overlay: bool,
    /// 启动即开启音频转发（需要 Android 10+ 的内录）
    pub audio: bool,
    /// 连上后（或按 Ctrl+G）启动的应用包名
    pub start_app: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            serial: None,
            max_size: 0,
            bit_rate: 8_000_000,
            max_fps: Some(60.0),
            stay_awake: false,
            turn_screen_off: false,
            no_control: false,
            list_devices: false,
            window_title: None,
            fullscreen: false,
            adb_path: None,
            play_file: None,
            dump_frame: None,
            ui_overlay: false,
            audio: true,
            start_app: None,
        }
    }
}

pub const USAGE: &str = "\
scrcpy-rs —— 用 Rust 重写的 scrcpy 客户端

用法: scrcpy-rs [选项]

设备:
  -s, --serial <序列号>     指定设备（默认自动选择唯一在线设备）
  -d, --list-devices        列出已连接设备后退出

视频:
  -m, --max-size <像素>     限制视频最长边（如 1024），默认原分辨率
  -b, --bit-rate <bps>      视频码率，默认 8000000
      --stay-awake          保持设备常亮
      --turn-screen-off     启动后关闭设备屏幕
      --no-control          只接收画面，不发送输入
      --max-fps <fps>       最大采集帧率（默认 60；写 0 表示不限，可能积压）
      --no-audio            关闭音频转发（默认开启，需要 Android 10+）
      --start-app <包名>     连上后启动该应用（Ctrl+G 可再触发）

窗口:
      --overlay             启动即叠加显示界面层级（同 Ctrl+U）
  -t, --window-title <文本> 自定义窗口标题
  -f, --fullscreen          启动即全屏

调试:
      --play-file <文件>    直接播放本地 Annex-B 裸流（不连设备，用于自测）
      --dump-frame <文件>   把第 20 帧导出为 BMP（自测渲染链路）
      --adb <路径>          指定 adb 可执行文件
  -h, --help                显示本帮助
  -V, --version             显示版本

快捷键:
  鼠标左键拖动 = 触摸滑动      右键 = 返回        中键 = Home
  滚轮 = 滚动                  Ctrl+R = 旋转设备
  Ctrl+H = Home               Ctrl+B = 返回
  Ctrl+P = 电源键             Ctrl+O = 息屏/亮屏
  Ctrl+U = 界面层级叠加开关    （每 2 秒 uiautomator dump 一次）
  Ctrl+Q = 退出                F11 = 全屏切换
  Ctrl+V = 粘贴到设备          Ctrl+C/X = 从设备复制/剪切
  Ctrl+N / Ctrl+Shift+N = 通知栏 / 快捷设置   Ctrl+Shift+C = 收起
  Ctrl+↑/↓ = 音量              Ctrl+M = 静音   Ctrl+S = 多任务
  Ctrl+Shift+R = 重置视频      Ctrl+G = 启动 --start-app 的应用
";

fn parse_num<T: std::str::FromStr>(v: &str, what: &str) -> Result<T> {
    v.parse::<T>()
        .map_err(|_| Error::Msg(format!("参数 {what} 的值不合法: {v}")))
}

impl Options {
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Option<Options>> {
        let mut o = Options::default();
        let mut it = args.into_iter().peekable();

        macro_rules! next {
            ($flag:expr) => {
                it.next()
                    .ok_or_else(|| Error::Msg(format!("{} 缺少参数值", $flag)))?
            };
        }

        while let Some(a) = it.next() {
            match a.as_str() {
                "-h" | "--help" => {
                    print!("{USAGE}");
                    return Ok(None);
                }
                "-V" | "--version" => {
                    println!("scrcpy-rs {}", env!("CARGO_PKG_VERSION"));
                    return Ok(None);
                }
                "-s" | "--serial" => o.serial = Some(next!("--serial")),
                "-d" | "--list-devices" => o.list_devices = true,
                "-m" | "--max-size" => o.max_size = parse_num(&next!("--max-size"), "--max-size")?,
                "-b" | "--bit-rate" => o.bit_rate = parse_num(&next!("--bit-rate"), "--bit-rate")?,
                "--max-fps" => o.max_fps = Some(parse_num(&next!("--max-fps"), "--max-fps")?),
                "--stay-awake" => o.stay_awake = true,
                "--turn-screen-off" => o.turn_screen_off = true,
                "--no-control" => o.no_control = true,
                "--no-audio" => o.audio = false,
                "--start-app" => o.start_app = Some(next!("--start-app")),
                "-t" | "--window-title" => o.window_title = Some(next!("--window-title")),
                "-f" | "--fullscreen" => o.fullscreen = true,
                "--play-file" => o.play_file = Some(PathBuf::from(next!("--play-file"))),
                "--dump-frame" => o.dump_frame = Some(PathBuf::from(next!("--dump-frame"))),
                "--overlay" => o.ui_overlay = true,
                "--adb" => o.adb_path = Some(PathBuf::from(next!("--adb"))),
                other => return Err(Error::Msg(format!("未知参数: {other}\n\n{USAGE}"))),
            }
        }
        Ok(Some(o))
    }
}
