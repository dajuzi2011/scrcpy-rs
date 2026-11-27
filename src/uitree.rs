//! 抓取并解析 Android 的界面层级（uiautomator dump），供窗口叠加绘制使用。
//!
//! 设计要点：
//! - **单次 adb 往返**：`exec-out sh -c '...dump...; cat'` 把「dump + 读回」合成一条命令，
//!   比先 `shell dump` 再 `pull` 快一倍，也少创建两个进程。
//! - **零依赖解析**：uiautomator 的输出是格式固定的扁平 `<node ...>` 列表，
//!   手写一个只认 `<node>` / `</node>` 的扫描器就够了，不需要引入 XML 库。
//! - 解析结果只保留绘制需要的东西（矩形、层级深度、类名、文本、是否可点击），
//!   每秒一次、几百个节点，开销可忽略。

use crate::adb::Adb;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.r - self.l
    }
    pub fn height(&self) -> i32 {
        self.b - self.t
    }
}

#[derive(Debug, Clone)]
pub struct UiNode {
    pub rect: Rect,
    /// 在层级树里的深度，根节点为 0
    pub depth: u16,
    pub class: String,
    pub text: String,
    pub res_id: String,
    pub clickable: bool,
    pub scrollable: bool,
}

#[derive(Debug, Clone)]
pub struct UiSnapshot {
    pub nodes: Vec<UiNode>,
    /// 设备屏幕尺寸（由根节点 bounds 得出），用于把设备坐标换算到视频坐标
    pub screen: (i32, i32),
}

impl UiSnapshot {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

const DUMP_CMD: &str = "rm -f /sdcard/ui.xml; \
                        uiautomator dump /sdcard/ui.xml >/dev/null 2>&1; \
                        cat /sdcard/ui.xml 2>/dev/null; true";

/// 让设备 dump 一次界面层级并解析。失败时返回可读的错误信息。
pub fn dump(adb: &Adb, serial: &str) -> Result<UiSnapshot> {
    let out = adb.capture(Some(serial), &["exec-out", "sh", "-c", DUMP_CMD])?;
    if !out.contains("<hierarchy") {
        return Err(Error::Msg(
            "uiautomator 没有返回层级（屏幕可能已锁定，或当前界面不可被无障碍服务读取）".into(),
        ));
    }
    parse(&out)
}

/// 解析 uiautomator 的 XML。只关心 `<node>` 元素。
pub fn parse(xml: &str) -> Result<UiSnapshot> {
    let bytes = xml.as_bytes();
    let mut nodes: Vec<UiNode> = Vec::new();
    let mut depth: i32 = -1;
    let mut screen: Option<(i32, i32)> = None;
    let mut saw_hierarchy = false;

    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // 找到标签结尾（跳过引号内的 '>'）
        let end = match find_tag_end(xml, i + 1) {
            Some(e) => e,
            // 末尾被截断的标签：忽略剩余内容
            None => break,
        };
        let tag = &xml[i + 1..end];
        i = end + 1;

        if let Some(rest) = tag.strip_prefix('/') {
            if rest.trim_start().starts_with("node") {
                depth -= 1;
            }
            continue;
        }
        if tag.starts_with('?') || tag.starts_with('!') {
            continue; // XML 声明 / 注释
        }

        let name_end = tag
            .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .unwrap_or(tag.len());
        let name = &tag[..name_end];

        match name {
            "hierarchy" => saw_hierarchy = true,
            "node" => {
                depth += 1;
                let self_closing = tag.trim_end().ends_with('/');
                if let Some(rect) = attr(tag, "bounds").and_then(parse_bounds) {
                    if depth == 0 {
                        screen = Some((rect.r.max(rect.l), rect.b.max(rect.t)));
                    }
                    nodes.push(UiNode {
                        rect,
                        depth: depth.max(0) as u16,
                        class: short_class(attr(tag, "class").unwrap_or("")),
                        text: unescape(attr(tag, "text").unwrap_or("")),
                        res_id: attr(tag, "resource-id").unwrap_or("").to_string(),
                        clickable: attr(tag, "clickable") == Some("true"),
                        scrollable: attr(tag, "scrollable") == Some("true"),
                    });
                }
                if self_closing {
                    depth -= 1;
                }
            }
            _ => {}
        }
    }

    if !saw_hierarchy {
        return Err(Error::Msg("返回内容不是 uiautomator 的 hierarchy".into()));
    }
    // 兜底：根节点没有 bounds 时，用所有节点的最大右下角推算屏幕尺寸
    let screen = screen.or_else(|| {
        let mut w = 0;
        let mut h = 0;
        for n in &nodes {
            w = w.max(n.rect.r);
            h = h.max(n.rect.b);
        }
        (w > 0 && h > 0).then_some((w, h))
    });

    let Some(screen) = screen else {
        return Err(Error::Msg("层级里没有可用的 bounds".into()));
    };
    // 微信这类应用会返回一个「空壳根节点」：命令成功、但 bounds 全 0 且没有子节点，
    // 也就是把整棵视图树对无障碍服务屏蔽了。这种情况必须报错，否则会被当成抓取成功。
    let all_degenerate = nodes
        .iter()
        .all(|n| n.rect.width() <= 0 || n.rect.height() <= 0);
    if screen.0 <= 0 || screen.1 <= 0 || all_degenerate {
        return Err(Error::Msg(
            "界面层级为空：目标应用禁用了无障碍访问（微信/支付宝等常见）".into(),
        ));
    }
    Ok(UiSnapshot { nodes, screen })
}

/// 从 `<` 之后找到标签结束的 `>`，跳过属性值里的引号内容
fn find_tag_end(xml: &str, from: usize) -> Option<usize> {
    let bytes = xml.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = from;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == b'"' || c == b'\'' {
                    quote = Some(c);
                } else if c == b'>' {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// 取属性值：`key="value"`。要求 key 前面是空白，避免 `resource-id` 命中 `id`。
fn attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let b = tag.as_bytes();
    let k = key.as_bytes();
    let mut i = 0usize;
    while i + k.len() < b.len() {
        let at_boundary = i == 0 || b[i - 1].is_ascii_whitespace();
        if at_boundary && b[i..].starts_with(k) {
            let after = i + k.len();
            if b[after] == b'=' && after + 1 < b.len() && (b[after + 1] == b'"' || b[after + 1] == b'\'')
            {
                let q = b[after + 1];
                let start = after + 2;
                if let Some(rel) = b[start..].iter().position(|&c| c == q) {
                    return Some(&tag[start..start + rel]);
                }
                return None;
            }
        }
        i += 1;
    }
    None
}

/// `[l,t][r,b]` -> Rect
fn parse_bounds(v: &str) -> Option<Rect> {
    let v = v.trim();
    let mut parts = v.split("][");
    let lt = parts.next()?.trim_start_matches('[');
    let rb = parts.next()?.trim_end_matches(']');
    let mut a = lt.split(',');
    let l = a.next()?.trim().parse().ok()?;
    let t = a.next()?.trim().parse().ok()?;
    let mut c = rb.split(',');
    let r = c.next()?.trim().parse().ok()?;
    let b = c.next()?.trim().parse().ok()?;
    if r < l || b < t {
        return None;
    }
    Some(Rect { l, t, r, b })
}

/// 只保留类名最后一段：`android.widget.TextView` -> `TextView`
fn short_class(full: &str) -> String {
    match full.rsplit('.').next() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => String::new(),
    }
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let (rep, consumed) = if tail.starts_with("&amp;") {
            ("&", 5)
        } else if tail.starts_with("&lt;") {
            ("<", 4)
        } else if tail.starts_with("&gt;") {
            (">", 4)
        } else if tail.starts_with("&quot;") {
            ("\"", 6)
        } else if tail.starts_with("&apos;") {
            ("'", 6)
        } else if tail.starts_with("&#10;") || tail.starts_with("&#xA;") {
            (" ", 5)
        } else if tail.starts_with("&#9;") {
            (" ", 4)
        } else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        out.push_str(rep);
        rest = &tail[consumed..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>
<hierarchy rotation="0">
  <node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.android.settings" content-desc="" clickable="false" enabled="true" scrollable="false" bounds="[0,0][1200,2670]">
    <node index="0" text="设置" resource-id="com.android.settings:id/title" class="android.widget.TextView" package="com.android.settings" clickable="true" enabled="true" scrollable="false" bounds="[48,120][300,220]">
      <node index="0" text="Wi-Fi &amp; 网络" resource-id="" class="android.widget.TextView" package="com.android.settings" clickable="false" enabled="true" scrollable="true" bounds="[48,220][1152,300]" />
    </node>
  </node>
</hierarchy>"#;

    #[test]
    fn parses_nodes_and_depth() {
        let snap = parse(SAMPLE).unwrap();
        assert_eq!(snap.screen, (1200, 2670));
        assert_eq!(snap.nodes.len(), 3);

        assert_eq!(snap.nodes[0].depth, 0);
        assert_eq!(snap.nodes[0].class, "FrameLayout");
        assert_eq!(snap.nodes[0].rect, Rect { l: 0, t: 0, r: 1200, b: 2670 });

        // 子节点深度递增，自闭合标签不会让后续深度错位
        assert_eq!(snap.nodes[1].depth, 1);
        assert_eq!(snap.nodes[1].class, "TextView");
        assert_eq!(snap.nodes[1].text, "设置");
        assert!(snap.nodes[1].clickable);
        assert_eq!(snap.nodes[1].res_id, "com.android.settings:id/title");

        assert_eq!(snap.nodes[2].depth, 2);
        assert_eq!(snap.nodes[2].text, "Wi-Fi & 网络");
        assert!(snap.nodes[2].scrollable);
    }

    /// 微信的真实返回：命令成功，但只有一个空壳根节点（bounds 全 0、没有子节点）
    #[test]
    fn rejects_empty_shell_hierarchy() {
        let wechat = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" resource-id="" class="" package="com.tencent.mm" content-desc="" checkable="false" checked="false" clickable="false" enabled="false" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,0][0,0]" drawing-order="0" hint="" /></hierarchy>"#;
        let err = parse(wechat).unwrap_err().to_string();
        assert!(err.contains("无障碍"), "错误信息应指向无障碍屏蔽: {err}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("ERROR: could not get idle state").is_err());
        assert!(parse("<hierarchy rotation=\"0\"></hierarchy>").is_err());
    }

    #[test]
    fn attr_requires_word_boundary() {
        // "id" 不能命中 "resource-id"
        let tag = r#"node resource-id="a:id/x" clickable="true""#;
        assert_eq!(attr(tag, "id"), None);
        assert_eq!(attr(tag, "resource-id"), Some("a:id/x"));
        assert_eq!(attr(tag, "clickable"), Some("true"));
    }

    #[test]
    fn bounds_parse() {
        assert_eq!(parse_bounds("[10,20][30,40]"), Some(Rect { l: 10, t: 20, r: 30, b: 40 }));
        assert_eq!(parse_bounds("[30,40][10,20]"), None);
        assert_eq!(parse_bounds(""), None);
    }
}
