//! adb 封装：所有与设备通信的底层操作都走这里。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use crate::error::{Error, Result};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone)]
pub struct Device {
    pub serial: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct Adb {
    exe: PathBuf,
}

impl Adb {
    /// 按优先级探测 adb：显式指定 > 程序目录 tools/adb > 环境变量 ADB > PATH
    pub fn discover(explicit: Option<&Path>) -> Result<Adb> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(p) = explicit {
            candidates.push(p.to_path_buf());
        }
        // 从可执行文件所在目录逐级往上找 tools/adb/（兼容 cargo run 时的 target/debug）
        if let Ok(exe) = std::env::current_exe() {
            let mut dir = exe.parent();
            for _ in 0..4 {
                if let Some(d) = dir {
                    candidates.push(d.join("tools").join("adb").join("adb.exe"));
                    candidates.push(d.join("adb.exe"));
                    dir = d.parent();
                } else {
                    break;
                }
            }
        }
        if cfg!(debug_assertions) {
            candidates.push(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tools")
                    .join("adb")
                    .join("adb.exe"),
            );
        }
        if let Ok(p) = std::env::var("ADB") {
            candidates.push(PathBuf::from(p));
        }
        candidates.push(PathBuf::from("adb.exe"));
        candidates.push(PathBuf::from("adb"));

        for c in candidates {
            let adb = Adb { exe: c };
            if adb.version().is_ok() {
                return Ok(adb);
            }
        }
        Err(Error::Msg(
            "找不到可用的 adb。请把 platform-tools 放到 ./tools/adb/，或用 --adb 指定路径".into(),
        ))
    }

    #[allow(dead_code)]
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.exe);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }

    pub fn version(&self) -> Result<String> {
        let out = self.command().arg("version").output()?;
        if !out.status.success() {
            return Err(Error::Msg("adb 执行失败".into()));
        }
        let s = String::from_utf8_lossy(&out.stdout).to_string();
        Ok(s.lines().next().unwrap_or("").trim().to_string())
    }

    /// 显式启动 adb server，避免首次调用时的长阻塞与竞态
    pub fn start_server(&self) -> Result<()> {
        let out = self.command().arg("start-server").output()?;
        if !out.status.success() {
            return Err(Error::Msg(format!(
                "adb start-server 失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        let out = self.command().arg("devices").output()?;
        if !out.status.success() {
            return Err(Error::Msg(format!(
                "adb devices 失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut list = Vec::new();
        for line in text.lines().skip(1) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('*') {
                continue;
            }
            let mut parts = line.split_whitespace();
            if let (Some(serial), Some(state)) = (parts.next(), parts.next()) {
                list.push(Device {
                    serial: serial.to_string(),
                    state: state.to_string(),
                });
            }
        }
        Ok(list)
    }

    /// 选择目标设备：优先指定序列号，否则要求恰好一台在线设备
    ///
    /// 刚执行过 start-server / 刚插线时，设备常常还处于 `offline`，
    /// 这里给它一点时间自行恢复，否则用户会看到莫名其妙的「没有可用设备」。
    pub fn select_device(&self, serial: Option<&str>) -> Result<String> {
        let start = std::time::Instant::now();
        loop {
            match self.select_device_once(serial) {
                Ok(s) => return Ok(s),
                Err(e) => {
                    let devices = self.devices().unwrap_or_default();
                    let waiting_for_recovery =
                        devices.iter().any(|d| d.state == "offline" || d.state == "unauthorized");
                    let empty = devices.is_empty();
                    // 有设备在恢复 -> 最多等 10s；列表为空 -> 只等 2s（多半是真没插）
                    if start.elapsed().as_secs() == 0 {
                        println!("[adb] 正在等待设备就绪…");
                    }
                    let limit = if waiting_for_recovery {
                        std::time::Duration::from_secs(12)
                    } else if empty {
                        // adb 服务刚重启时，USB 设备要重新枚举，实测能到 20s 以上
                        std::time::Duration::from_secs(30)
                    } else {
                        std::time::Duration::from_secs(0)
                    };
                    if start.elapsed() >= limit {
                        return Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        }
    }

    fn select_device_once(&self, serial: Option<&str>) -> Result<String> {
        let devices = self.devices()?;
        let online: Vec<&Device> = devices.iter().filter(|d| d.state == "device").collect();

        if let Some(s) = serial {
            if online.iter().any(|d| d.serial == s) {
                return Ok(s.to_string());
            }
            // 设备存在但未授权时给出更明确的提示
            if let Some(d) = devices.iter().find(|d| d.serial == s) {
                return Err(Error::Msg(format!(
                    "设备 {s} 当前状态是「{}」，请在手机上确认 USB 调试授权",
                    d.state
                )));
            }
            return Err(Error::Msg(format!("找不到序列号为 {s} 的设备")));
        }

        match online.len() {
            0 => {
                let hint = if devices.iter().any(|d| d.state == "offline") {
                    "设备处于 offline 状态，正在等待 adb 重新连接…"
                } else if devices.iter().any(|d| d.state == "unauthorized") {
                    "设备未授权，请在手机屏幕上点「允许 USB 调试」"
                } else {
                    "没有可用设备。请用数据线连接手机，开启「开发者选项 → USB 调试」并授权本机"
                };
                Err(Error::Msg(hint.into()))
            }
            1 => Ok(online[0].serial.clone()),
            _ => {
                let names: Vec<&str> = online.iter().map(|d| d.serial.as_str()).collect();
                Err(Error::Msg(format!(
                    "检测到多台设备，请用 -s 指定其中之一: {}",
                    names.join(", ")
                )))
            }
        }
    }

    fn run(&self, serial: Option<&str>, args: &[&str]) -> Result<Output> {
        let mut cmd = self.command();
        if let Some(s) = serial {
            cmd.arg("-s").arg(s);
        }
        cmd.args(args);
        let out = cmd.output()?;
        Ok(out)
    }

    /// 执行 adb 子命令并返回 stdout，失败时把 stderr 作为错误抛出
    pub fn capture(&self, serial: Option<&str>, args: &[&str]) -> Result<String> {
        let out = self.run(serial, args)?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let err = if err.is_empty() {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            } else {
                err
            };
            return Err(Error::Msg(format!("adb {} 失败: {err}", args.join(" "))));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub fn push(&self, serial: &str, local: &Path, remote: &str) -> Result<()> {
        let out = self.run(
            Some(serial),
            &["push", &local.to_string_lossy(), remote],
        )?;
        if !out.status.success() {
            return Err(Error::Msg(format!(
                "推送 server 失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    /// 远程文件的字节数；不存在时返回 None
    pub fn remote_size(&self, serial: &str, remote: &str) -> Option<u64> {
        let out = self
            .run(Some(serial), &["shell", &format!("stat -c %s {remote}")])
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }

    pub fn forward(&self, serial: &str, local_port: u16, remote: &str) -> Result<()> {
        self.capture(
            Some(serial),
            &[
                "forward",
                &format!("tcp:{local_port}"),
                &format!("localabstract:{remote}"),
            ],
        )?;
        Ok(())
    }

    pub fn forward_remove(&self, serial: &str, local_port: u16) {
        let port = format!("tcp:{local_port}");
        let _ = self.run(Some(serial), &["forward", "--remove", &port]);
    }

    /// 启动一个长时间运行的 adb shell 进程（server 本体），stdout/stderr 用管道接回来
    pub fn spawn_shell(&self, serial: &str, args: &[String]) -> Result<Child> {
        let mut cmd = self.command();
        cmd.arg("-s").arg(serial).arg("shell");
        for a in args {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd.spawn()?)
    }
}
