//! 邮件配置加载与持久化:读取 config.yaml, 并在运行时写回 last_ok_interval.
//!
//! 使用逐行解析(不引入 YAML 依赖), 避免重型依赖, 减小二进制体积;
//! 同时在更新 `last_ok_interval` 时保留注释与文件结构.

use std::fs;
use std::path::{Path, PathBuf};
use tracing::warn;

/// 服务器不支持 IDLE 时的兜底轮询间隔(秒)--错过推送后最多多久才能再次取到.
pub const FALLBACK: u64 = 30;
/// 指数退避上限(秒)--轮询/重连间隔最大值.
pub const MAX_POLL_INTERVAL: u64 = 300;
/// 每次错误时的退避倍率.
pub const BACKOFF_FACTOR: f64 = 1.5;
/// 启动检查窗口:只认为最近多少秒内的邮件是"新"的(秒).
pub const STARTUP_RECENT_SECONDS: i64 = 300;

/// 单个邮件账号.
pub struct Account {
    pub email: String,
    pub label: Option<String>,
    pub server: String,
    pub port: u16,
    pub password: String,
    pub poll_interval: u64,
    pub last_ok_interval: Option<u64>,
}

impl Account {
    /// 显示名:设置了 label 就用 label, 否则用 email.
    pub fn label_or_email(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.email)
    }
}

/// 顶层邮件配置:验证码匹配关键词 + 账号列表.
pub struct MailConfig {
    pub match_keywords: Vec<String>,
    pub accounts: Vec<Account>,
    /// 未命中关键词的新邮件是否也弹通知.默认 true(保持旧行为:每封新邮件都提醒).
    pub notify_unmatched: bool,
}

/// 默认配置文件路径--exe 同目录的 config.yaml.
pub fn default_config_file() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("config.yaml")))
        .unwrap_or_else(|| PathBuf::from("config.yaml"))
}

/// 首次显式指定路径但文件不存在时写入的示例内容(用户明确提供了路径).
pub const SAMPLE_YAML: &str = "\
# ============================================================
# 邮件监听配置  (config.yaml)
#
# match_keywords: 邮件(标题+发件人+正文)命中任意关键词(忽略大小写)
#                 就会触发验证码自动提取.直接在这里增删即可, 不用改源码
#                 再重新编译.用 `match_keywords: []` 可显式关闭关键词触发.
#
# notify_unmatched: true/false, 未命中关键词的新邮件是否也弹通知.
#                  设为 false 则只有命中关键词(含验证码提取)的邮件才提醒,
#                  可避免 GitHub 这类通知邮件刷屏.默认 true.
#

# accounts: 下每个账号的字段说明
#   password     : 邮箱的 IMAP 授权码, 不是网页登录密码
#   port         : IMAP over SSL 端口, 一般为 993
#   poll_interval: 基础轮询间隔(秒);服务器不支持 IDLE 时使用
#   last_ok_interval: 程序自动维护的上次成功间隔, 重启后保留
# ============================================================

match_keywords:
  - \"验证码\"
  - \"校验码\"
  - \"动态码\"
  - \"一次性代码\"
  - \"verification code\"
  - \"verify code\"
  - \"login code\"
#  - \"OTP\"
#  - \"security code\"

notify_unmatched: true

accounts:
  # ---- QQ 示例(支持 IMAP IDLE, 实时推送)----
  - email: \"your_qq@qq.com\"
    label: \"QQ\"
    server: imap.qq.com
    port: 993
    password: \"YOUR_QQ_IMAP_AUTH_CODE\"

  # ---- 126 示例(无 IDLE, 使用 poll_interval 轮询)----
  - email: \"your_126@126.com\"
    label: \"126\"
    server: imap.126.com
    password: \"YOUR_126_IMAP_AUTH_CODE\"
    poll_interval: 30

  # ---- sina 示例 ----
  - email: \"your_sina@sina.com\"
    label: \"sina\"
    server: imap.sina.com
    password: \"YOUR_SINA_IMAP_AUTH_CODE\"
    poll_interval: 30

  # ---- Gmail 示例(支持 IDLE;需用 16 位应用专用密码)----
  - email: \"your@gmail.com\"
    label: \"Gmail\"
    server: imap.gmail.com
    port: 993
    password: \"YOUR_16CHAR_GOOGLE_APP_PASSWORD\"
";

/// 加载完整邮件配置.
///
/// - `custom_path = Some(p)`:从 p 读取;若文件不存在, 写入示例并返回
///   `MailConfig { 示例关键词, 空账号 }`.
/// - `custom_path = None`:读取默认路径(exe 同目录 config.yaml).
///   **若默认文件不存在, 直接返回"空关键词 + 0 账号", 不做任何写入** --
///   文件不存在表示用户不启用邮件监听, 避免产生副作用和额外线程.
pub fn load_config(custom_path: Option<&Path>) -> MailConfig {
    let (path, explicit) = match custom_path {
        Some(p) => (p.to_path_buf(), true),
        None => (default_config_file(), false),
    };
    if !path.exists() {
        if explicit {
            match fs::write(&path, SAMPLE_YAML) {
                Ok(_) => {
                    tracing::info!("未找到配置文件 {:?}，已写入示例；请编辑后重启程序", path);
                }
                Err(e) => {
                    warn!("写入示例 {:?} 失败: {}；跳过邮件监听", path, e);
                }
            }
        }
        return MailConfig {
            match_keywords: Vec::new(),
            accounts: Vec::new(),
            notify_unmatched: true,
        };
    }
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            warn!("读取 {:?} 失败: {}；跳过邮件监听", path, e);
            return MailConfig {
                match_keywords: Vec::new(),
                accounts: Vec::new(),
                notify_unmatched: true,
            };
        }
    };
    parse_config(&text)
}

/// 解析完整 config.yaml:可选的 `match_keywords:` 列表 + `accounts:` 列表.
fn parse_config(text: &str) -> MailConfig {
    let match_keywords = parse_match_keywords(text).unwrap_or_default();
    let accounts = parse_accounts(text);
    let notify_unmatched = parse_notify_unmatched(text).unwrap_or(true);
    MailConfig {
        match_keywords,
        accounts,
        notify_unmatched,
    }
}

/// 找到顶层 `notify_unmatched:` 键(缩进=0), 读取其布尔值.
/// 若键不存在则返回 `None`, 由调用方按默认(true)处理.
fn parse_notify_unmatched(text: &str) -> Option<bool> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("notify_unmatched:") {
            let ind = line.len() - line.trim_start().len();
            if ind == 0 {
                let v = trimmed.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
                if v.is_empty() {
                    return None;
                }
                return Some(parse_bool(v));
            }
        }
    }
    None
}

/// 找到顶层 `match_keywords:` 键(缩进=0), 读取其 `- "值"` 列表.
/// 若键完全不存在则返回 `None`, 由调用方按默认(空)处理.
fn parse_match_keywords(text: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut key_idx = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with("match_keywords:") {
            let ind = line.len() - line.trim_start().len();
            if ind == 0 {
                key_idx = Some(i);
                break;
            }
        }
    }
    let key_idx = key_idx?;
    let key_line = lines[key_idx];
    let inline_val = key_line
        .split_once(':')
        .map(|(_, v)| v.trim())
        .unwrap_or("");
    // 允许 `match_keywords: []` 显式清空列表
    if !inline_val.is_empty() {
        let v = inline_val.trim();
        if v == "[]" {
            return Some(Vec::new());
        }
    }

    let mut list_indent: Option<usize> = None;
    let mut out: Vec<String> = Vec::new();

    for line in lines.iter().skip(key_idx + 1) {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();

        // 到了顶层非空行, 说明已离开 match_keywords 块
        if indent == 0 {
            break;
        }

        if trimmed.starts_with("- ") {
            match list_indent {
                None => list_indent = Some(indent),
                Some(li) if indent != li => {
                    if indent < li {
                        break;
                    }
                    continue;
                }
                _ => {}
            }
            let raw = &trimmed[2..];
            let cleaned = clean_value(raw);
            if !cleaned.is_empty() {
                out.push(cleaned);
            }
        } else if list_indent.is_none() {
            // 还没遇到列表项, 跳过缩进的非列表行
            continue;
        } else {
            // 已经开始列表了;一个缩进 <= 列表缩进的非列表行意味着块结束
            if let Some(li) = list_indent {
                if indent <= li {
                    break;
                }
            }
        }
    }
    Some(out)
}

/// 逐行解析 `accounts` 列表;容忍注释, 空行和引号.
fn parse_accounts(text: &str) -> Vec<Account> {
    let lines: Vec<&str> = text.lines().collect();
    // 找顶层 `accounts:` 键(缩进=0)
    let mut accounts_idx = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t == "accounts:" || t.starts_with("accounts:") {
            let ind = line.len() - line.trim_start().len();
            if ind == 0 {
                accounts_idx = Some(i);
                break;
            }
        }
    }
    let start = match accounts_idx {
        Some(i) => i + 1,
        None => return Vec::new(),
    };

    let mut accounts: Vec<Account> = Vec::new();
    let mut dash_indent: Option<usize> = None;
    let mut current: Option<Account> = None;

    for line in lines.iter().skip(start) {
        let stripped = line.trim();
        // 跳过空行和注释;账号之间的注释不能打断列表
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();

        // 顶层非空行 → accounts 列表结束
        if indent == 0 {
            if let Some(acc) = current.take() {
                accounts.push(acc);
            }
            break;
        }

        // 列表项:`  - email: ...`
        let trimmed = line.trim_start();
        if trimmed.starts_with("- ") {
            // 第一个 `- ` 决定列表项缩进;只有同级才算新账号
            match dash_indent {
                None => dash_indent = Some(indent),
                Some(di) => {
                    if indent < di {
                        break;
                    }
                    if indent != di {
                        continue;
                    }
                }
            }
            if let Some(acc) = current.take() {
                accounts.push(acc);
            }
            let rest = &trimmed[2..]; // 去掉 "- "
            let mut acc = Account {
                email: String::new(),
                label: None,
                server: String::new(),
                port: 993,
                password: String::new(),
                poll_interval: FALLBACK,
                last_ok_interval: None,
            };
            if let Some((k, v)) = split_kv(rest) {
                apply_field(&mut acc, k, v);
            }
            current = Some(acc);
            continue;
        }

        // 字段行:缩进必须严格大于 dash_indent 才算属于当前账号
        if let Some(acc) = current.as_mut() {
            let di = dash_indent.unwrap_or(0);
            if indent > di {
                if let Some((k, v)) = split_kv(trimmed) {
                    apply_field(acc, k, v);
                }
                continue;
            }
            // 缩进 <= dash_indent → 当前账号块结束
            accounts.push(current.take().unwrap());
            break;
        } else {
            // 还没进入列表项就遇到同级或更小的非空行 → 停止
            if indent == 0 {
                break;
            }
        }
    }
    if let Some(acc) = current.take() {
        accounts.push(acc);
    }

    // 只保留 server + password 都非空的账号(示例占位也算;登录失败在连接时再报)
    accounts
        .into_iter()
        .filter(|a| !a.server.is_empty() && !a.password.is_empty())
        .collect()
}

/// 切分 `key: value` → `(key_trimmed, value_raw)`;没有冒号返回 None.
fn split_kv(s: &str) -> Option<(&str, &str)> {
    let (k, v) = s.split_once(':')?;
    Some((k.trim(), v))
}

/// 清理 YAML 原始值:去行内注释, 去引号, 去首尾空白.
fn clean_value(raw: &str) -> String {
    let v = raw.trim();
    // 双引号:取引号之间内容, 内部 `#` 保留
    if v.starts_with('"') {
        if let Some(end) = v[1..].find('"') {
            return v[1..1 + end].to_string();
        }
    }
    if v.starts_with('\'') {
        if let Some(end) = v[1..].find('\'') {
            return v[1..1 + end].to_string();
        }
    }
    // 行内注释:第一个未加引号的 ` ` + `#`(或行首 `#`)
    let mut out = v;
    if let Some(pos) = find_inline_comment(v) {
        out = &v[..pos];
    }
    out.trim().to_string()
}

/// 找行内注释的起始位置(` #` 或行首 `#`).
fn find_inline_comment(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut in_quotes = false;
    let mut quote = b'"';
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_quotes {
            if b == quote {
                in_quotes = false;
            }
        } else if b == b'"' || b == b'\'' {
            in_quotes = true;
            quote = b;
        } else if b == b'#' {
            if i == 0 || bytes[i - 1].is_ascii_whitespace() {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn parse_bool(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

fn apply_field(acc: &mut Account, key: &str, raw_value: &str) {
    let value = clean_value(raw_value);
    match key {
        "email" => acc.email = value,
        "label" => acc.label = if value.is_empty() { None } else { Some(value) },
        "server" => acc.server = value,
        "port" => {
            if let Ok(p) = value.parse::<u16>() {
                acc.port = p;
            }
        }
        "password" => acc.password = value,
        "poll_interval" => {
            if let Ok(p) = value.parse::<u64>() {
                acc.poll_interval = p.max(1);
            }
        }
        "last_ok_interval" => {
            if let Ok(p) = value.parse::<u64>() {
                acc.last_ok_interval = Some(p);
            }
        }
        _ => {}
    }
}

/// 把当前账号无错误的轮询间隔持久化写回 config.yaml.
/// 按 `email` 匹配账号, 在原地新增或更新 `last_ok_interval` 字段(保留注释).
/// 失败只记录日志, 不影响监听循环.
pub fn save_account_interval(email_key: &str, interval: u64) {
    let path = default_config_file();
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            warn!("写回 last_ok_interval 失败(读取){:?}: {}", path, e);
            return;
        }
    };
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();

    // 定位账号块:形如 `- email: ...<email_key>...` 的列表项
    let mut start = None;
    let mut dash_indent = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let s = line.trim_start();
        if s.starts_with("- ") && s.contains("email:") && line.contains(email_key) {
            start = Some(i);
            dash_indent = line.len() - line.trim_start().len();
            break;
        }
    }
    let start = match start {
        Some(i) => i,
        None => return,
    };
    let field_indent = dash_indent + 2;

    // 块边界:下一个同级 `- ` 项, 更小缩进的非空行, 或顶层键
    let mut end = lines.len();
    for i in (start + 1)..lines.len() {
        if lines[i].trim().is_empty() {
            continue;
        }
        let ind = lines[i].len() - lines[i].trim_start().len();
        if ind <= dash_indent {
            end = i;
            break;
        }
    }

    let key = "last_ok_interval:";
    let new_line = format!(
        "{}last_ok_interval: {}   # 由程序维护：上次正常运行的间隔",
        " ".repeat(field_indent),
        interval
    );
    let mut replaced = false;
    for i in (start + 1)..end {
        if lines[i].trim_start().starts_with(key) {
            lines[i] = new_line.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.insert(end, new_line);
    }

    let out = lines.join("\n");
    let tmp = path.with_extension("yaml.tmp");
    if let Err(e) = fs::write(&tmp, &out) {
        warn!("写回 last_ok_interval 失败(写临时文件){:?}: {}", path, e);
        return;
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        warn!("写回 last_ok_interval 失败(重命名){:?}: {}", path, e);
    }
}
