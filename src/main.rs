//! scrcpy-rs —— 用 Rust 从零实现的 scrcpy 主机端。
//!
//! 与官方 scrcpy 的关系：设备端仍然必须运行一个 Java server（Android 的 MediaCodec
//! 只有 Java API），本项目复用官方 server jar，但主机端的协议解析、解码调度、渲染、
//! 输入注入全部用 Rust 重新实现。

mod adb;
mod audio;
mod bmp;
mod app;
mod cli;
mod clipboard;
mod control;
mod error;
mod hid;
mod mf;
mod panel;
mod record;
mod server;
mod screenshot;
mod sps;
mod uitree;
mod video;
mod window;
mod yuv;

use error::Result;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match cli::Options::parse(args) {
        Ok(Some(o)) => o,
        Ok(None) => return,
        Err(e) => {
            eprintln!("参数错误: {e}");
            std::process::exit(2);
        }
    };

    if let Err(e) = run(opts) {
        eprintln!("错误: {e}");
        std::process::exit(1);
    }
}

fn run(opts: cli::Options) -> Result<()> {
    app::set_dump_path(opts.dump_frame.clone());
    mf::startup()?;
    let result = run_inner(opts);
    mf::shutdown();
    result
}

fn run_inner(opts: cli::Options) -> Result<()> {
    // 调试模式：不连设备，直接回放裸流
    if let Some(path) = opts.play_file.clone() {
        return app::run_file(&path, &opts);
    }

    let adb = adb::Adb::discover(opts.adb_path.as_deref())?;
    println!("[adb] {}", adb.version()?);

    if opts.list_devices {
        let devices = adb.devices()?;
        if devices.is_empty() {
            println!("没有检测到设备");
        }
        for d in devices {
            println!("{:<24} {}", d.serial, d.state);
        }
        return Ok(());
    }

    let session = server::start(adb, &opts)?;
    app::run_device(session, &opts)
}
