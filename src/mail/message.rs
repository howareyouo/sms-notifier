//! MIME 解析与验证码提取:把原始 RFC822 字节解析为结构化消息, 提取纯文本正文,
//! 并在命中验证码关键字时抽取 4-6 位数字验证码.
//!
//! 手写实现以避免引入 mail/imap 等 MIME crate, 缩小打包体积.

use base64::Engine;
use encoding::all::GB18030;
use encoding::{DecoderTrap, Encoding};
use tracing::warn;

pub struct ParsedMessage {
    pub subject: String,
    pub from: String,
    pub body: String,
}

/// 解析原始 RFC822 字节为结构化消息.失败时返回尽可能完整的字段.
pub fn parse(raw: &[u8]) -> ParsedMessage {
    // 头部与正文以第一个空行(\r\n\r\n 或 \n\n)分隔.
    let (head_bytes, body_bytes) = split_head_body(raw);
    let headers = parse_headers(head_bytes);
    let subject = decode_mime_header(get_header(&headers, "subject").unwrap_or_default().trim());
    let from = decode_mime_header(get_header(&headers, "from").unwrap_or_default().trim());
    let body = extract_body(&headers, body_bytes);
    ParsedMessage {
        subject,
        from,
        body,
    }
}

/// 检查 text 是否包含任意关键词(忽略大小写).
pub fn has_match_keyword(text: &str, keywords: &[String]) -> bool {
    let low = text.to_lowercase();
    keywords.iter().any(|k| low.contains(&k.to_lowercase()))
}

/// 提取验证码:优先匹配更长的(6 > 5 > 4 位), 且要求不被更长数字串包含.
/// 命中返回验证码字符串;未命中返回 None.
pub fn extract_code(text: &str) -> Option<String> {
    // 收集所有"最大数字串"及其长度.
    let bytes = text.as_bytes();
    let mut runs: Vec<(usize, usize)> = Vec::new(); // (start, len)
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            runs.push((start, i - start));
        } else {
            i += 1;
        }
    }
    // 6 > 5 > 4:依次找首个该长度的串.
    for n in [6usize, 5, 4] {
        for &(start, len) in &runs {
            if len == n {
                return Some(text[start..start + n].to_string());
            }
        }
    }
    None
}

// ───────────────────────── 头部解析 ─────────────────────────

/// 头部键名(小写)→ 原始值(含折行已拼接).
fn parse_headers(head: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(head);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur_key: Option<String> = None;
    let mut cur_val = String::new();

    let flush = |out: &mut Vec<(String, String)>, key: &mut Option<String>, val: &mut String| {
        if let Some(k) = key.take() {
            out.push((k, std::mem::take(val)));
        }
    };

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        // 折行:以空格/Tab 开头, 追加到当前值.
        if line.starts_with(' ') || line.starts_with('\t') {
            cur_val.push(' ');
            cur_val.push_str(line.trim());
            continue;
        }
        flush(&mut out, &mut cur_key, &mut cur_val);
        if let Some((k, v)) = line.split_once(':') {
            cur_key = Some(k.trim().to_lowercase());
            cur_val.push_str(v.trim_start());
        } else {
            cur_key = Some(String::new());
            cur_val.push_str(line);
        }
    }
    flush(&mut out, &mut cur_key, &mut cur_val);
    out
}

fn get_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// 头部与正文的分界:首个 \r\n\r\n 或 \n\n.
fn split_head_body(raw: &[u8]) -> (&[u8], &[u8]) {
    for i in 0..raw.len().saturating_sub(3) {
        if &raw[i..i + 4] == b"\r\n\r\n" {
            return (&raw[..i], &raw[i + 4..]);
        }
    }
    for i in 0..raw.len().saturating_sub(1) {
        if &raw[i..i + 2] == b"\n\n" {
            return (&raw[..i], &raw[i + 2..]);
        }
    }
    (raw, &[])
}

// ───────────────────────── 正文提取 ─────────────────────────

/// 提取邮件纯文本正文(优先 text/plain, 否则退回 text/html 去标签).
fn extract_body(headers: &[(String, String)], body: &[u8]) -> String {
    // Content-Type 的媒体类型比较需要 lowercase, 但取 boundary/charset 参数必须
    // 保留原始大小写, 因为 multipart 的 boundary 分隔线是大小写敏感的字面量
    // (RFC 2046 5.1.1 "exactly the same octet sequence"), 如果此处把 boundary 值
    // lowercased 再去 walk_multipart 比对, 分隔线含大写字母(如 ..._Part_...)时
    // 会一个 part 都匹配不到, 最后只能 fallback 吐出 base64 原文.
    let ct_raw = get_header(headers, "content-type").unwrap_or_default();
    let ct_lc = ct_raw.to_lowercase();
    let boundary = header_param(ct_raw, "boundary").map(str::to_owned);
    let top_charset = header_param(ct_raw, "charset").map(str::to_owned);

    let mut text: Option<String> = None;
    let mut html: Option<String> = None;

    fn pick_text(
        pheaders: &[(String, String)],
        cte_decoded: Vec<u8>,
        text: &mut Option<String>,
        html: &mut Option<String>,
    ) {
        let pct = get_header(pheaders, "content-type")
            .unwrap_or_default()
            .to_lowercase();
        let pcharset = header_param(&pct, "charset");
        // 每个 part 按自身 Content-Type 的 charset 解码 (multipart 顶层一般不带 charset).
        let decoded_str = decode_charset(&cte_decoded, pcharset);
        if pct.contains("text/plain") && text.is_none() {
            *text = Some(decoded_str);
        } else if pct.contains("text/html") && html.is_none() {
            *html = Some(decoded_str);
        } else if text.is_none() && html.is_none() && !decoded_str.trim().is_empty() {
            // 未声明类型但非空:兜底当纯文本.
            *text = Some(decoded_str);
        }
    }

    if let Some(boundary) = boundary {
        let parts = walk_multipart(body, &boundary);
        if parts.is_empty() && !body.is_empty() {
            // boundary 没匹配到任何 part, 做兜底回退:把整个 body 按顶层 charset 解码.
            warn!(
                "[extract_body] boundary={:?} 未匹配到任何 part, body_len={}, 回退整段解码",
                boundary,
                body.len()
            );
            let s = decode_charset(body, top_charset.as_deref());
            if ct_lc.contains("text/html") {
                html = Some(s);
            } else if !s.trim().is_empty() {
                text = Some(s);
            }
        }
        for part in parts.iter() {
            let (part_headers, part_body) = split_head_body(part);
            let pheaders = parse_headers(part_headers);
            // 取原始 Content-Type 用于提取 boundary (保留大小写); pct_lc 只用于判断媒体类型.
            let pct_raw = get_header(&pheaders, "content-type").unwrap_or_default();
            let pct_lc = pct_raw.to_lowercase();
            if pct_lc.contains("multipart/") {
                // 嵌套 multipart:先 CTE 解码, 再拆内层 parts.
                let cte_decoded = decode_part(&pheaders, part_body);
                // 关键:boundary 从原始 pct_raw 取, 不能从 lowercased 的 pct_lc 取,
                // 否则分隔线里的大写字母会被改小写, walk_multipart 一个都匹配不上.
                if let Some(nb) = header_param(pct_raw, "boundary") {
                    let inners = walk_multipart(&cte_decoded, nb);
                    for inner in inners {
                        let (ih, ib) = split_head_body(&inner);
                        let ihdrs = parse_headers(ih);
                        let idecoded = decode_part(&ihdrs, ib);
                        pick_text(&ihdrs, idecoded, &mut text, &mut html);
                    }
                } else if !cte_decoded.is_empty() {
                    // 声明了 multipart/ 但没带 boundary, 退化解码当文本.
                    pick_text(&pheaders, cte_decoded, &mut text, &mut html);
                }
            } else {
                let cte_decoded = decode_part(&pheaders, part_body);
                pick_text(&pheaders, cte_decoded, &mut text, &mut html);
            }
        }
    } else if ct_lc.contains("text/html") {
        let cte_decoded = decode_part(headers, body);
        html = Some(decode_charset(&cte_decoded, top_charset.as_deref()));
    } else {
        let cte_decoded = decode_part(headers, body);
        let s = decode_charset(&cte_decoded, top_charset.as_deref());
        text = Some(s);
    }

    // 优先 text/plain;若为空白则退回 text/html.
    if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
        return clean_text(&t);
    }
    if let Some(h) = html.filter(|h| !h.trim().is_empty()) {
        return strip_html(&h);
    }
    String::new()
}

/// 按 boundary 切分 multipart 正文, 返回各部分原始字节(含各自头部).
///
/// 容错处理:
/// - boundary 匹配是 ASCII 大小写不敏感的 (双保险, 主要 boundary 来自原始 header,
///   应保持大小写一致; 但若 upstream 错用 lowercased boundary 也不会全 miss).
/// - boundary 行前后可能有空格/分号参数 (如 `--boundary; boundary-parameter`),
///   用 starts_with 匹配而非严格相等.
/// - 终止线 `--boundary--` 同样允许尾随空格/参数.
/// - 首部 preamble 与尾部 epilogue 自动忽略.
fn walk_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let dash = format!("--{}", boundary);
    let term = format!("--{}--", boundary);
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut start: Option<usize> = None;
    let bytes = body;

    fn starts_with_ci(haystack: &str, needle: &str) -> bool {
        if needle.len() > haystack.len() {
            return false;
        }
        haystack[..needle.len()].eq_ignore_ascii_case(needle)
    }

    fn matches_boundary(line: &str, dash: &str, term: &str) -> (bool, bool) {
        // 先去 CRLF, 再 trim 尾空格(容忍末尾空白/分号参数).
        let line = line.trim_end_matches(['\r', '\n', ' ', '\t']);
        // 终止线优先判断, 因为 term 以 dash 开头, 会被 dash 的 starts_with 覆盖.
        if starts_with_ci(line, term) {
            return (false, true);
        }
        if starts_with_ci(line, dash) {
            // 必须严格是分隔线:dash 之后是 EOL、空白或 `;`/参数. 排除 `--boundaryfoo`.
            let rest = &line[dash.len()..];
            if rest.is_empty()
                || rest.starts_with(|c: char| {
                    c == ' ' || c == '\t' || c == ';' || c == '\r' || c == '\n'
                })
            {
                return (true, false);
            }
        }
        (false, false)
    }

    // 逐行扫描 boundary 行.
    let mut i = 0;
    while i < bytes.len() {
        let j = match find_lf(bytes, i) {
            Some(j) => j,
            None => {
                // 末尾行(缺尾换行), 同样比对一次.
                let line = &bytes[i..];
                let line_str = String::from_utf8_lossy(line);
                let (is_dash, is_term) = matches_boundary(&line_str, &dash, &term);
                if is_dash {
                    if let Some(s) = start {
                        parts.push(&bytes[s..i]);
                    }
                    start = Some(bytes.len()); // dash 之后无内容, 但仍标记起始(空 part)
                } else if is_term {
                    if let Some(s) = start {
                        parts.push(&bytes[s..i]);
                    }
                    start = None;
                }
                break;
            }
        };
        let line = &bytes[i..j];
        let line_str = String::from_utf8_lossy(line);
        let (is_dash, is_term) = matches_boundary(&line_str, &dash, &term);
        if is_dash {
            if let Some(s) = start {
                parts.push(&bytes[s..i]);
            }
            start = Some(j + 1);
        } else if is_term {
            if let Some(s) = start {
                parts.push(&bytes[s..i]);
            }
            start = None;
            break;
        }
        i = j + 1;
    }
    if let Some(s) = start {
        // 缺少显式终止线; 直到末尾都算最后一个 part.
        parts.push(&bytes[s..]);
    }
    parts
}

fn find_lf(bytes: &[u8], from: usize) -> Option<usize> {
    bytes[from..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| from + p)
}

/// 解码单个 MIME part:处理 Content-Transfer-Encoding, 返回解码后的字节.
fn decode_part(headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let cte = get_header(headers, "content-transfer-encoding")
        .unwrap_or_default()
        .to_lowercase();
    match cte.trim() {
        "base64" => decode_base64_bytes(body),
        "quoted-printable" => decode_qp(body),
        _ => body.to_vec(), // 7bit / 8bit / binary / 未知
    }
}

/// 解码 base64(容忍空白与软换行).
fn decode_base64_bytes(body: &[u8]) -> Vec<u8> {
    let filtered: Vec<u8> = body
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(&filtered)
        .unwrap_or_else(|_| filtered.clone())
}

/// 解码 quoted-printable.
fn decode_qp(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        let b = body[i];
        if b == b'=' {
            // 软换行 =\r\n 或 =\n
            if i + 1 < body.len() && body[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            if i + 2 < body.len() && body[i + 1] == b'\r' && body[i + 2] == b'\n' {
                i += 3;
                continue;
            }
            // =XX
            if i + 2 < body.len() {
                if let (Some(hi), Some(lo)) = (hex(body[i + 1]), hex(body[i + 2])) {
                    out.push(hi * 16 + lo);
                    i += 3;
                    continue;
                }
            }
            out.push(b);
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    out
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 从 Content-Type 值中取参数(boundary, charset).
/// 例:`multipart/mixed; boundary="----=_Part"` → Some("----=_Part")
/// 参数名查找忽略大小写, 容错 `Boundary=` `CHARSET=` 这类异写.
fn header_param<'a>(ct: &'a str, name: &str) -> Option<&'a str> {
    // 1) 优先按 `name=` 精确查
    let needle = format!("{}=", name);
    if let Some(pos) = ct.find(&needle) {
        let after = &ct[pos + needle.len()..];
        return Some(extract_param_value(after));
    }
    // 2) 退化为大小写不敏感扫描
    let bytes = ct.as_bytes();
    let n = name.len();
    if n == 0 || n > bytes.len() {
        return None;
    }
    let mut i = 0;
    while i + n <= bytes.len() {
        if bytes[i + n - 1] == b'=' && ct[i..i + n - 1].eq_ignore_ascii_case(name) {
            // 边界检查:前一个字符必须是 ';' 或空白 (避免误吃子串).
            if i == 0 {
                let after = &ct[i + n..];
                return Some(extract_param_value(after));
            }
            let prev = ct.as_bytes()[i - 1];
            if prev == b';' || prev.is_ascii_whitespace() {
                let after = &ct[i + n..];
                return Some(extract_param_value(after));
            }
        }
        i += 1;
    }
    None
}

fn extract_param_value(after: &str) -> &str {
    if let Some(rest) = after.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return &rest[..end];
        }
    }
    let end = after
        .find(|c: char| c == ';' || c.is_ascii_whitespace())
        .unwrap_or(after.len());
    &after[..end]
}

/// 用声明编码解码, 失败回退 gb18030 → utf-8 lossy, 避免中文乱码.
/// 只引用 `encoding::all::GB18030` 静态表(GBK 为其子集), 配合 LTO
/// 其余编码表会被链接器丢弃, 不会增大二进制.
fn decode_charset(raw: &[u8], charset: Option<&str>) -> String {
    match charset.and_then(classify_charset) {
        Some(Charset::Utf8) => String::from_utf8_lossy(raw).into_owned(),
        Some(Charset::Latin1) => raw.iter().map(|&b| b as char).collect(),
        Some(Charset::Gbk) | None => gbk_decode(raw),
    }
}

/// 把 charset label 归类为三种我们支持的解码方式. 未列出(如 big5/shift_jis)
/// 返回 None, 由调用方回退到 GB18030 → UTF-8 lossy.
fn classify_charset(label: &str) -> Option<Charset> {
    let l = label.to_ascii_lowercase();
    let l = l.trim().trim_matches('"').trim();
    match l {
        "utf-8" | "utf8" | "us-ascii" | "ascii" => Some(Charset::Utf8),
        "iso-8859-1" | "latin1" | "windows-1252" | "cp1252" | "iso8859-1" => {
            Some(Charset::Latin1)
        }
        "gbk" | "gb2312" | "gb18030" | "gb_2312" | "x-gbk" | "hz" => Some(Charset::Gbk),
        _ => None,
    }
}

/// 按 GB18030(兼容 GBK)解码;失败退回 UTF-8 lossy, 保证绝不 panic.
fn gbk_decode(raw: &[u8]) -> String {
    GB18030
        .decode(raw, DecoderTrap::Replace)
        .unwrap_or_else(|_| String::from_utf8_lossy(raw).into_owned())
}

#[derive(Clone, Copy)]
enum Charset {
    Utf8,
    Latin1,
    Gbk,
}

fn clean_text(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let s = fold_blank_lines(&s);
    let s: String = s
        .split('\n')
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    s.trim().to_string()
}

/// 折叠连续空行:把 `\n[ \t]*\n[ \t\n]*` 替换为单个 `\n`.
/// 与原 `\n[ \t]*\n[ \t\n]*` 正则语义一致, 手写实现避免 regex 依赖.
fn fold_blank_lines(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            // 跳过开头的 [ \t]*, 至少需要再遇到一个 \n 才算空行.
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'\n' {
                // 命中空行:输出单个 \n, 跳过整段 [ \t\n]*.
                out.push('\n');
                i = j + 1;
                while i < bytes.len() {
                    let b = bytes[i];
                    if b == b' ' || b == b'\t' || b == b'\n' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

// ───────────────────────── HTML 处理 ─────────────────────────

fn strip_html(html_text: &str) -> String {
    let clean = strip_tags(html_text);
    let unescaped = unescape_html(&clean);
    unescaped
        .split('\n')
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 剥除所有 HTML 标签, 与原 `(?si)<style[^>]*>.*?</style>|<script[^>]*>.*?</script>|<[^>]+>` 语义一致.
/// 手写字节扫描, 避免 regex 依赖.
fn strip_tags(html_text: &str) -> String {
    let bytes = html_text.as_bytes();
    let mut out = String::with_capacity(html_text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // 先尝试整段 <style>...</style> / <script>...</script>.
            if let Some(end) = try_strip_block(html_text, i, b"<style", b"</style>") {
                i = end;
                continue;
            }
            if let Some(end) = try_strip_block(html_text, i, b"<script", b"</script>") {
                i = end;
                continue;
            }
            // 普通标签: <...>.
            if let Some(close) = find_subseq(html_text, i, b">") {
                i = close + 1;
                continue;
            }
            // 未闭合的 < : 原样保留.
            out.push('<');
            i += 1;
            continue;
        }
        let ch = html_text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 尝试匹配并整段移除 `<open_prefix...>...</close_tag>`.
/// 全部按 ASCII 大小写不敏感比较.失败返回 None (调用方应退化为普通标签处理).
fn try_strip_block(
    text: &str,
    start: usize,
    open_prefix: &[u8],
    close_tag: &[u8],
) -> Option<usize> {
    let bytes = text.as_bytes();
    if start + open_prefix.len() > bytes.len() {
        return None;
    }
    if !bytes[start..start + open_prefix.len()].eq_ignore_ascii_case(open_prefix) {
        return None;
    }
    // 紧跟字符必须为非字母, 避免误吃 `<styles>` 这类自定义标签.
    if let Some(next) = text[start + open_prefix.len()..].chars().next() {
        if next.is_ascii_alphabetic() {
            return None;
        }
    }
    let open_end = find_subseq(text, start, b">")?;
    let close_start = find_subseq_ci(text, open_end + 1, close_tag)?;
    Some(close_start + close_tag.len())
}

/// 大小写敏感查找子字节序列.
fn find_subseq(text: &str, from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > text.len() {
        return None;
    }
    let bytes = text.as_bytes();
    let end = bytes.len() - needle.len();
    let from = from.min(end + 1);
    for i in from..=end {
        if &bytes[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

/// ASCII 大小写不敏感查找子字节序列.
fn find_subseq_ci(text: &str, from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > text.len() {
        return None;
    }
    let bytes = text.as_bytes();
    let end = bytes.len() - needle.len();
    let from = from.min(end + 1);
    for i in from..=end {
        if bytes[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(i);
        }
    }
    None
}

/// 反转义常见 HTML 实体.
fn unescape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = s[i..].find(';') {
                let entity = &s[i + 1..i + semi];
                if let Some(c) = decode_entity(entity) {
                    out.push(c);
                    i += semi + 1;
                    continue;
                }
            }
            out.push('&');
            i += 1;
        } else {
            // 安全推进一个 char(UTF-8 边界)
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            if let Some(num) = entity.strip_prefix('#') {
                let code =
                    if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok()?
                    } else {
                        num.parse::<u32>().ok()?
                    };
                char::from_u32(code)
            } else {
                None
            }
        }
    }
}

// ───────────────────────── 编码字(encoded-word)解码 ─────────────────────────

/// 解码 MIME 头部中的编码字 `=?charset?B?...?=` / `=?charset?Q?...?=`,
/// 其余原文保留.相邻编码字之间的空白按 RFC 2047 丢弃.
fn decode_mime_header(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut last_was_encoded = false;

    while i < bytes.len() {
        // 编码字以 `=?` 开头.
        if bytes[i] == b'=' && i + 1 < bytes.len() && bytes[i + 1] == b'?' {
            if let Some(end) = find_encoded_word_end(&value[i + 2..]) {
                let inner = &value[i + 2..i + 2 + end];
                if let Some(decoded) = decode_encoded_word(inner) {
                    out.push_str(&decoded);
                    i += 2 + end;
                    last_was_encoded = true;
                    continue;
                }
            }
        }
        // 编码字后的线性空白且下一个仍是编码字:丢弃该空白.
        let ch = value[i..].chars().next().unwrap();
        if last_was_encoded && ch.is_whitespace() {
            let mut j = i + ch.len_utf8();
            let mut found_ew = false;
            while j < bytes.len() {
                let c = value[j..].chars().next().unwrap();
                if c.is_whitespace() {
                    j += c.len_utf8();
                    continue;
                }
                if c == '=' && j + 1 < bytes.len() && bytes[j + 1] == b'?' {
                    found_ew = true;
                }
                break;
            }
            if found_ew {
                i += ch.len_utf8();
                continue;
            }
        }
        out.push(ch);
        i += ch.len_utf8();
        last_was_encoded = false;
    }
    out
}

/// 在 `s`(已去掉开头 `=?`)中找 `?=` 的位置, 返回到 `?=` 末尾的长度.
fn find_encoded_word_end(s: &str) -> Option<usize> {
    // 结构 charset ? enc ? text ?=
    let bytes = s.as_bytes();
    let q1 = bytes.iter().position(|&b| b == b'?')?;
    let rest = &s[q1 + 1..];
    let bytes2 = rest.as_bytes();
    let q2 = bytes2.iter().position(|&b| b == b'?')?;
    let text = &rest[q2 + 1..];
    let q3 = text.find("?=")?;
    Some(q1 + 1 + q2 + 1 + q3 + 2)
}

/// 解码单个编码字内容(charset?enc?text).
fn decode_encoded_word(inner: &str) -> Option<String> {
    let (charset, rest) = inner.split_once('?')?;
    let (enc, text) = rest.split_once('?')?;
    let text = text.strip_suffix("?=").unwrap_or(text);
    let raw = match enc.to_ascii_uppercase().as_str() {
        "B" => decode_base64_bytes(text.as_bytes()),
        "Q" => decode_qp_word(text),
        _ => return None,
    };
    let encoding = classify_charset(charset)?;
    let decoded = match encoding {
        Charset::Utf8 => String::from_utf8_lossy(&raw).into_owned(),
        Charset::Latin1 => raw.iter().map(|&b| b as char).collect(),
        Charset::Gbk => GB18030.decode(&raw, DecoderTrap::Replace).ok()?,
    };
    Some(decoded)
}

/// Q 编码:`_` 表示空格, 其余按 quoted-printable.
fn decode_qp_word(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'_' {
            out.push(b' ');
            i += 1;
        } else if b == b'=' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
            out.push(b);
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    out
}
