# scrcpy-rs

用 Rust 从零重写的 **scrcpy 主机端**（Windows 单平台，零第三方 crate）。

协议解析、H.264 解码调度、渲染、输入注入全部自己实现，不依赖 scrcpy 的 C 客户端，也不依赖 FFmpeg / SDL。

设备端仍使用官方 `scrcpy-server.jar` v5.0.1：Android 的屏幕采集只能走 `MediaCodec`（Java API），任何"纯 Rust 重写"都绕不过这一层。jar 通过 `include_bytes!` 内嵌进二进制，运行时自动推送。

---

## 快速开始

```bash
# 编译
cargo build --release

# 手机开启「开发者选项 → USB 调试」，插上数据线
./target/release/scrcpy-rs.exe

# 指定设备 / 限制分辨率与码率
./target/release/scrcpy-rs.exe -s <序列号> -m 1024 -b 4000000 --max-fps 60
```

adb 查找顺序：`--adb 路径` → 可执行文件附近的 `tools/adb/adb.exe` → 环境变量 `ADB` → `PATH`。

**关于 `--max-fps`**：默认 60。不限制时，120Hz 手机会按屏幕刷新率推 ~120fps，而本机在 1200×2670 下解码 + 色彩转换约 98fps 上限，差出来的帧会堆在 socket 里，延迟一路涨到秒级（表面现象是"触控立刻到、画面半天才变"）。若延迟仍异常，说明还在积压，把帧率调低即可。

## 功能

| 能力 | 说明 |
|---|---|
| 投屏 | H.264 硬解（Media Foundation），窗口缩放 / 全屏（F11）/ 无边框 |
| 音频 | 服务端 `audio_codec=raw` → 48kHz 立体声 PCM，直接 `waveOut` 播放，无需解码器 |
| 触摸 / 键鼠 | 绝对坐标触摸、滚轮、完整键码表；可选 UHID 虚拟键盘与相对鼠标 |
| 剪贴板 | 双向同步，支持「粘贴到设备」「从设备复制/剪切」 |
| 中文输入 | 非 ASCII 自动改走剪贴板粘贴（`INJECT_TEXT` 只能映射键盘布局上存在的字符） |
| 文件拖拽 | 拖进窗口即 `adb push` 到 `/sdcard/Download/`，并刷新媒体库 |
| 截图 / 录屏 | PNG（WIC 编码）/ H.264，有 ffmpeg 时自动转 MP4 |
| 界面层级叠加 | 每 2 秒 `uiautomator dump`，把控件边界按层级着色叠在画面上 |
| 断线重连 | 1→2→4→8→16→30s 退避，窗口、面板、开关全部保留 |
| 采集参数 | 分辨率 / 码率 / 帧率面板可调，「应用并重连」生效 |

## 控制面板

窗口右侧常驻一块自绘面板（纯 GDI，不建子窗口控件）。左侧分隔条可拖动调宽（240~460px），画面自动等比缩放并居中。

```
控制面板
──────────────────────
画面
[ 截图 ]      [ 录制 ]
[ 打开目录 ]  [ 音频 开 ]
刷新上限   [−]  60 fps  [+]

控制
[ 层级叠加        开启 ]
[ 剪贴板同步      关闭 ]
[ 粘贴到设备 ][ 从设备复制 ]
[ UHID 键盘       关闭 ]
[ 相对鼠标        关闭 ]
[ 平滑缩放        开启 ]
把文件拖进窗口即可推送到手机

采集参数（改完需重连）
分辨率   [−]   720 p   [+]
码率     [−]  8 Mbps   [+]
帧率     [−]    60     [+]
[        应用并重连        ]

状态
```

| 控件 | 行为 |
|---|---|
| 截图 | 当前上屏帧存成 PNG（WIC 编码），落在 `scrcpy-rs-out/` |
| 录制 / 停止录制 | 存 H.264，结束时有 ffmpeg 就转成 MP4 |
| 音频 | 开关声音（服务端始终转发，这里只控制是否出声） |
| 刷新上限 | 30/60/90/120/不限，只节流「转换 + 上屏」，不动码流 |
| 层级叠加 | 与 `Ctrl+U` 等效 |
| 剪贴板同步 | 双向同步，关闭时两个方向都停 |
| UHID 键盘 | 模拟真实键盘，组合键（Ctrl+A/C/V）在设备内生效，需 Android 9+ |
| 相对鼠标 | 只发位移增量，指针不撞边界、可拖动/游戏，`Esc` 退出 |
| 平滑缩放 | 关掉走最近邻（文字更锐），打开走 2×2 盒式平均（更干净），两者都是 1~2ms |
| 分辨率/码率/帧率 | 改的是「待应用」值，点「应用并重连」才生效 |
| 拖文件进窗口 | `adb push` 到 `/sdcard/Download/`，并用 `SCAN_FILE` 刷新媒体库 |

## 操作方式

| 操作 | 效果 |
|---|---|
| 鼠标左键拖动 | 触摸 / 滑动 |
| 鼠标右键 / 中键 | 返回 / Home |
| 滚轮 | 上下滚动 |
| 键盘输入 | 注入按键（A-Z、0-9、方向键、退格、回车…） |
| 中文输入 | 自动改走剪贴板粘贴 |
| `Ctrl+R` / `Ctrl+H` / `Ctrl+B` | 旋转 / Home / 返回 |
| `Ctrl+P` / `Ctrl+O` | 电源键 / 息屏切换 |
| `Ctrl+V` / `Ctrl+C` / `Ctrl+X` | 粘贴到设备 / 从设备复制 / 剪切 |
| `Ctrl+N` / `Ctrl+Shift+N` / `Ctrl+Shift+C` | 通知栏 / 快捷设置 / 收起面板 |
| `Ctrl+↑` `Ctrl+↓` / `Ctrl+M` / `Ctrl+S` | 音量加减 / 静音 / 多任务 |
| `Ctrl+Shift+R` | 重置视频编码器（花屏时） |
| `Ctrl+U` | 界面层级叠加开关 |
| `Ctrl+G` | 启动 `--start-app` 指定的应用 |
| `Esc` | 退出相对鼠标捕获 |
| `Ctrl+Q` / `F11` | 退出 / 全屏切换 |

---

## 协议实现（对照 v5.0.1 源码逐条核对）

**启动**

```
adb push scrcpy-server.jar /data/local/tmp/
adb forward tcp:<port> localabstract:scrcpy_<scid:08x>
adb shell CLASSPATH=/data/local/tmp/scrcpy-server.jar app_process / \
    com.genymobile.scrcpy.Server 5.0.1 scid=... log_level=info \
    tunnel_forward=true control=true [max_size=..] [video_bit_rate=..] [max_fps=..]
```

- 三个 socket 复用同一个 adb forward，服务端按 **video → audio → control** 顺序 accept
- 只有第一条连接（video）会先收到 **1 字节 dummy**，用于确认隧道真的通了
- 视频 socket：`64B 设备名` → `4B codec id` → 若干 `12B 包头 + 数据`

**视频包头（12 字节）**

```
最高位为 1 → session 包：[flags(4)][width(4)][height(4)]
最高位为 0 → 媒体包：  [pts|flags(8)][size(4)][payload(size)]
                       bit62 = config(SPS/PPS)，bit61 = 关键帧
```

**控制消息（client → device）**

| type | 消息 | 载荷 |
|---|---|---|
| 0 | INJECT_KEYCODE | action(1) keycode(4) repeat(4) meta(4) |
| 1 | INJECT_TEXT | len(4) + UTF-8 |
| 2 | INJECT_TOUCH_EVENT | action(1) pointerId(8) x(4) y(4) w(2) h(2) pressure(u16 定点) actionButton(4) buttons(4) |
| 3 | INJECT_SCROLL_EVENT | x(4) y(4) w(2) h(2) hscroll(i16 定点) vscroll(i16) buttons(4) |
| 4 / 5 / 6 / 7 | BACK_OR_SCREEN_ON / 通知栏 / 快捷设置 / 收起面板 | action(1) 或无 |
| 8 | GET_CLIPBOARD | copy_key(1) |
| 9 | SET_CLIPBOARD | sequence(8) paste(1) len(4) + UTF-8 |
| 10 | SET_DISPLAY_POWER | on(1) |
| 11 | ROTATE_DEVICE | 无 |
| 12 | UHID_CREATE | id(2) vendor(2) product(2) name_len(1) name desc_len(2) desc |
| 13 | UHID_INPUT | id(2) len(2) + data |
| 14 | UHID_DESTROY | id(2) |
| 16 | START_APP | len(**1**) + 包名 |
| 17 | RESET_VIDEO | 无 |
| 22 | SCAN_FILE | len(4) + 路径 |

**设备消息（device → client）**：`0` 剪贴板（len(4)+UTF-8）、`1` ACK_CLIPBOARD（sequence 8B）、`2` UHID_OUTPUT（id(2) len(2) data）。

---

## 代码结构

```
src/
├── main.rs      入口、参数分发
├── cli.rs       命令行解析
├── error.rs     统一错误类型
├── adb.rs       adb 封装（设备发现 / push / forward / shell）
├── server.rs    server 部署、隧道建立、三 socket 握手
├── video.rs     视频流解复用（12 字节包头）
├── sps.rs       H.264 SPS 解析（取分辨率，含裁剪）
├── mf.rs        Media Foundation H.264 解码器封装
├── yuv.rs       NV12 → BGRA（按行并行）
├── window.rs    Win32 窗口、消息循环、输入事件、绘制与缩放
├── control.rs   控制协议编码
├── hid/         UHID 键盘 / 相对鼠标的报文与描述符
├── uitree.rs    uiautomator 抓取 + XML 解析（层级叠加数据源）
├── panel.rs     右侧控制面板（行模型自绘 + 命中测试）
├── audio.rs     waveOut 播放裸 PCM（48k 立体声）
├── clipboard.rs Windows 剪贴板读写 + 变化检测
├── record.rs    H.264 录屏（SinkWriter 尝试 + ffmpeg 转封装）
├── screenshot.rs WIC 编码 PNG 截图
├── bmp.rs       调试用 BMP 导出
└── app.rs       编排：连接 → 建窗 → 解码线程 → 输入映射
```

**线程模型**

```
主线程      Win32 消息循环 + 绘制（阻塞在 MsgWaitForMultipleObjectsEx）
解码线程    socket 读包 → MFT 解码 → NV12→BGRA → SetEvent 唤醒主线程
控制通道    主线程直接写 socket（set_nodelay，几十字节无阻塞风险）
层级线程    adb dump → 解析 → 整体替换快照 → InvalidateRect（开关关闭时空转）
音频线程    socket 读包 → 裸 PCM 入环形缓冲；播放线程按 20ms 送 waveOut
剪贴板线程  轮询序号变化 → SET_CLIPBOARD；控制通道反向读设备消息
推流线程    拖放文件 → adb push（不阻塞 UI）
```

帧缓冲是**双缓冲 + 回收池**：解码线程从池里取一块复用，UI 线程画完把旧块还回去，稳态下零分配。
视频缩放自己算（`scale_into`）：后备缓冲用 `CreateDIBSection` 拿 CPU 指针，按 2×2 盒式平均多线程写入，最后只做一次 `BitBlt`。用 GDI 的 `HALFTONE` 单帧要 17~35ms（上屏被压到 24fps），最近邻又会让色度采样错位出现彩边，只能自己写。

---

## 无设备时的自测

```bash
# 用 ffmpeg 造一段 H.264 裸流
ffmpeg -f lavfi -i testsrc2=size=720x1280:rate=30:duration=3 \
       -c:v libx264 -pix_fmt yuv420p -bsf:v h264_metadata=aud=insert \
       -f h264 testdata/test720p.h264

# 直接回放（与真机完全相同的解码 + 渲染链路）
./target/release/scrcpy-rs.exe --play-file testdata/test720p.h264

# 导出第 20 帧为 BMP，校验色彩/方向
./target/release/scrcpy-rs.exe --play-file testdata/test720p.h264 --dump-frame out.bmp

cargo test
```

单元测试覆盖 SPS 解析、NV12→BGRA 基准、握手与包头解析（用按字节模拟的假 server）、
面板布局/命中、缩放正确性（1:1 无损、纯色块、棋盘平均、越界裁剪）。

调试时可加 `SCRCPY_RS_DEBUG=1`，日志会给出 `[fps] 解码 Nfps / 队列滞后 M ms`、
`[ui] 取帧/绘制/平均耗时`、`[paint] 视频/面板分项耗时`。

---

## 常见问题（真机）

**① 触摸 / 按键完全没反应**

设备端日志会打印 `Injecting input events requires ... INJECT_EVENTS permission`。
这不是本项目的 bug，是系统权限：小米 / HyperOS、部分 ColorOS / OriginOS 需要额外打开
**设置 → 开发者选项 →「USB 调试（安全设置）」** 并重启手机（或重新插拔数据线）。
在此之前任何 scrcpy 客户端（包括官方版）都注入不了触摸。

**② 某些应用层级叠加没有框（微信 / 支付宝等）**

`uiautomator dump` 命令成功，但返回的 XML 只有一个空壳根节点（`bounds="[0,0][0,0]"`、无子节点）。
这些应用把整棵视图树对无障碍服务屏蔽了（`NO_HIDE_DESCENDANTS` 或自定义 `AccessibilityDelegate`），
属于应用侧的反自动化策略，主机端没有绕过手段。这种情况会被识别为失败并在面板显示原因。

**③ 画面卡顿 / 操作不跟手**

先看是否积压：`SCRCPY_RS_DEBUG=1` 下的 `队列滞后` 若持续增长，说明采集帧率高于解码能力，
把面板「帧率」调到 60（或用 `--max-fps 60`）。程序也会在滞后超过 1.2s 时自动降帧并重建会话。
另外「层级叠加」开着时，`uiautomator dump` 会让手机 UI 每 2 秒短暂卡一下，属设备端行为。

**④ 缩小的窗口有彩边 / 色彩分裂**

最近邻降采样会把 4:2:0 的色度采样点错位。本项目自己做的 2×2 盒式平均已经避免这个问题；
若手动关掉「平滑缩放」并放大观察，边缘出现轻微锯齿属正常。

**⑤ 画面频闪**

所有绘制先落在离屏位图上，最后一次 `BitBlt` 上屏，窗口上只会出现完整的旧帧或新帧。

---

## 已知限制

- 只支持 **H.264**（HEVC/AV1 需在 `mf.rs` 换 CLSID 与 subtype，协议层不用改）
- 只支持 **tunnel_forward**（adb forward），不支持 reverse 隧道
- 音频只有 `raw` PCM 通路（不做 Opus/AAC 解码，码率比官方高）
- 录屏转 MP4 依赖系统 ffmpeg；没有 ffmpeg 时保留 `.h264`
- 没有摄像头、多显示器、虚拟显示（`--new-display`）
- 服务端版本与 jar 必须严格一致：改 `server::SERVER_VERSION` 会导致启动失败

## 更新 server jar

jar 通过 `include_bytes!` 编译进二进制，换 jar 必须重新编译，并同步修改 `src/server.rs` 里的 `SERVER_VERSION`。
