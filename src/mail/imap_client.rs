//! 底层 IMAP 客户端:基于 native-tls 手写实现, 覆盖监听所需的命令
//! (LOGIN / SELECT / UID SEARCH / UID FETCH RFC822 / UID FETCH INTERNALDATE
//! / CAPABILITY / ID / IDLE+DONE).不引入 imap crate 以缩小体积.

use native_tls::TlsStream;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;
use tracing::warn;

#[derive(Debug)]
pub enum ImapError {
    Io(std::io::Error),
    Tls(native_tls::Error),
    /// 服务器返回 NO/BAD 或协议异常.
    Protocol(String),
    /// 登录失败(授权码错误等).
    Auth(String),
}

impl std::fmt::Display for ImapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImapError::Io(e) => write!(f, "IO: {}", e),
            ImapError::Tls(e) => write!(f, "TLS: {}", e),
            ImapError::Protocol(s) => write!(f, "协议: {}", s),
            ImapError::Auth(s) => write!(f, "登录: {}", s),
        }
    }
}

impl From<std::io::Error> for ImapError {
    fn from(e: std::io::Error) -> Self {
        ImapError::Io(e)
    }
}
impl From<native_tls::Error> for ImapError {
    fn from(e: native_tls::Error) -> Self {
        ImapError::Tls(e)
    }
}
impl From<native_tls::HandshakeError<TcpStream>> for ImapError {
    fn from(e: native_tls::HandshakeError<TcpStream>) -> Self {
        match e {
            native_tls::HandshakeError::Failure(err) => ImapError::Tls(err),
            native_tls::HandshakeError::WouldBlock(_) => ImapError::Io(std::io::Error::new(
                ErrorKind::WouldBlock,
                "TLS handshake would block on blocking stream",
            )),
        }
    }
}

/// IDLE 一次循环的结果.
pub enum IdleResult {
    /// 收到 `* N EXISTS` 推送, 有新邮件.
    Exists,
    /// 超时到期, 未收到推送(兜底轮询触发点).
    Timeout,
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct ImapClient {
    stream: TlsStream<TcpStream>,
    tag: u32,
    /// 读缓冲:TlsStream 只能按字节读, 行边界需自己拼.
    buf: Vec<u8>,
    capabilities: Vec<String>,
}

impl ImapClient {
    /// 建立 TLS 连接并完成 LOGIN + CAPABILITY.返回的客户端尚未 SELECT.
    pub fn connect(
        server: &str,
        port: u16,
        email: &str,
        password: &str,
    ) -> Result<Self, ImapError> {
        let addr_str = format!("{}:{}", server, port);
        let mut last_err: Option<std::io::Error> = None;
        let mut tcp: Option<TcpStream> = None;
        for a in addr_str.to_socket_addrs()? {
            match TcpStream::connect_timeout(&a, Duration::from_secs(15)) {
                Ok(s) => {
                    s.set_read_timeout(Some(DEFAULT_TIMEOUT)).ok();
                    s.set_write_timeout(Some(DEFAULT_TIMEOUT)).ok();
                    // TCP keepalive:IDLE 是长连接, NAT/防火墙会在空闲一段时间后丢弃表项,
                    // 导致下一次读写报 10060.每 10s 发一个空 ACK 探测包保活连接.
                    set_keepalive(&s);
                    tcp = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let tcp = tcp.ok_or_else(|| {
            ImapError::Io(last_err.unwrap_or_else(|| {
                std::io::Error::new(ErrorKind::Other, "无法解析/连接 IMAP 服务器")
            }))
        })?;

        let connector = native_tls::TlsConnector::builder().build()?;
        let stream = connector.connect(server, tcp)?;

        let mut client = ImapClient {
            stream,
            tag: 0,
            buf: Vec::new(),
            capabilities: Vec::new(),
        };

        // 读取服务器问候 `* OK ...`.
        let greeting = client.read_line()?;
        let g = String::from_utf8_lossy(&greeting);
        if !g.starts_with("* OK") {
            return Err(ImapError::Protocol(format!("非预期问候: {}", g.trim())));
        }

        client.login(email, password)?;
        client.capability()?;
        Ok(client)
    }

    /// 当前账号是否支持 IDLE.
    pub fn supports_idle(&self) -> bool {
        self.capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("IDLE"))
    }

    // ─────────────── 命令实现 ───────────────

    fn next_tag(&mut self) -> String {
        self.tag += 1;
        format!("A{}", self.tag)
    }

    fn send_raw(&mut self, data: &[u8]) -> Result<(), ImapError> {
        self.stream.write_all(data)?;
        self.stream.flush()?;
        Ok(())
    }

    /// 读一行(到 `\n`), 返回不含行尾的内容(同时去掉 `\r`).
    fn read_line(&mut self) -> Result<Vec<u8>, ImapError> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Err(ImapError::Io(std::io::Error::new(
                    ErrorKind::ConnectionAborted,
                    "IMAP 连接已关闭",
                )));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// 精确读取 n 字节(优先消耗缓冲).
    fn read_exact(&mut self, out: &mut [u8]) -> Result<(), ImapError> {
        let mut filled = 0;
        while filled < out.len() {
            if !self.buf.is_empty() {
                let take = std::cmp::min(self.buf.len(), out.len() - filled);
                out[filled..filled + take].copy_from_slice(&self.buf[..take]);
                self.buf.drain(..take);
                filled += take;
            } else {
                let n = self.stream.read(&mut out[filled..])?;
                if n == 0 {
                    return Err(ImapError::Io(std::io::Error::new(
                        ErrorKind::ConnectionAborted,
                        "IMAP 连接已关闭",
                    )));
                }
                filled += n;
            }
        }
        Ok(())
    }

    /// 发送一条带 tag 的命令, 读取到对应 tag 完成响应为止.
    /// 返回 (是否 OK, 所有未标记响应行).
    fn run_command(&mut self, cmd: &str) -> Result<(bool, Vec<String>), ImapError> {
        let tag = self.next_tag();
        let line = format!("{} {}\r\n", tag, cmd);
        self.send_raw(line.as_bytes())?;

        let mut untagged: Vec<String> = Vec::new();
        loop {
            let raw = self.read_line()?;
            let s = String::from_utf8_lossy(&raw).into_owned();
            if s.starts_with(tag.as_str()) {
                let rest = &s[tag.len()..];
                let ok = rest.trim_start().to_uppercase().starts_with("OK");
                return Ok((ok, untagged));
            }
            if s.starts_with('*') {
                untagged.push(s);
            }
            // `+` 延续响应在此处不应出现(非 IDLE 命令);忽略即可.
        }
    }

    fn login(&mut self, email: &str, password: &str) -> Result<(), ImapError> {
        // 不对密码做转义:授权码通常不含特殊字符;若含空格用引号包裹.
        let cmd = format!("LOGIN {} {}", quote(email), quote(password));
        let (ok, _) = self.run_command(&cmd).map_err(|e| match e {
            ImapError::Protocol(s) => ImapError::Auth(format!(
                "登录失败：请确认 config.yaml 中的 password 已替换为真实 IMAP 授权码（{}）",
                s
            )),
            other => other,
        })?;
        if !ok {
            return Err(ImapError::Auth(
                "登录失败：请确认 config.yaml 中的 password 已替换为真实 IMAP 授权码".to_string(),
            ));
        }
        Ok(())
    }

    fn capability(&mut self) -> Result<(), ImapError> {
        let (ok, untagged) = self.run_command("CAPABILITY")?;
        if !ok {
            return Ok(()); // CAPABILITY 失败不致命, 按不支持 IDLE 处理
        }
        for line in &untagged {
            if line.to_uppercase().starts_with("* CAPABILITY") {
                self.capabilities = line
                    .trim_start_matches("* CAPABILITY")
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect();
            }
        }
        Ok(())
    }

    /// 网易系邮箱要求登录后先发 IMAP ID 命令标识客户端, 否则 SELECT 拒绝.
    /// 失败仅告警, 不影响后续.
    pub fn send_id(&mut self) {
        let cmd = "ID (\"name\" \"Mail Listener\" \"version\" \"1.0\")";
        if let Err(e) = self.run_command(cmd) {
            warn!("发送 ID 命令失败（可忽略）: {}", e);
        }
    }

    pub fn select_inbox(&mut self) -> Result<(), ImapError> {
        let (ok, _) = self.run_command("SELECT INBOX")?;
        if !ok {
            return Err(ImapError::Protocol("select INBOX 失败".to_string()));
        }
        Ok(())
    }

    /// UID SEARCH ALL:返回邮箱内全部 UID(单调递增).
    pub fn uid_search_all(&mut self) -> Result<Vec<u32>, ImapError> {
        let (ok, untagged) = self.run_command("UID SEARCH ALL")?;
        if !ok {
            return Err(ImapError::Protocol("UID SEARCH 失败".to_string()));
        }
        for line in &untagged {
            let up = line.to_uppercase();
            if up.starts_with("* SEARCH") {
                let rest = line.trim_start_matches("* SEARCH");
                let mut uids: Vec<u32> = rest
                    .split_whitespace()
                    .filter_map(|t| t.parse::<u32>().ok())
                    .collect();
                uids.sort_unstable();
                return Ok(uids);
            }
        }
        Ok(Vec::new())
    }

    /// UID FETCH <uid> (RFC822):返回原始邮件字节, UID 不存在返回 None.
    pub fn uid_fetch_rfc822(&mut self, uid: u32) -> Result<Option<Vec<u8>>, ImapError> {
        let cmd = format!("UID FETCH {} (RFC822)", uid);
        let tag = self.next_tag();
        let line = format!("{} {}\r\n", tag, cmd);
        self.send_raw(line.as_bytes())?;

        let mut raw_body: Option<Vec<u8>> = None;
        loop {
            let raw = self.read_line()?;
            let s = String::from_utf8_lossy(&raw).into_owned();
            if s.starts_with(tag.as_str()) {
                let ok = s[tag.len()..].trim_start().to_uppercase().starts_with("OK");
                if !ok {
                    return Err(ImapError::Protocol(format!("FETCH 失败: {}", s.trim())));
                }
                return Ok(raw_body);
            }
            if s.starts_with('*') {
                // 行尾形如 `{1234}`, 其后紧跟字面量.
                if let Some(n) = parse_literal_len(&s) {
                    let mut body = vec![0u8; n];
                    self.read_exact(&mut body)?;
                    raw_body = Some(body);
                    // 消耗字面量后的 `)` 行.
                    let _trailing = self.read_line()?;
                }
            }
        }
    }

    /// UID FETCH <uid> (INTERNALDATE):返回服务器视角的收件时间字符串
    ///(形如 `17-Jul-2024 12:34:56 +0800`).取不到返回 None.
    pub fn uid_fetch_internaldate(&mut self, uid: u32) -> Result<Option<String>, ImapError> {
        let cmd = format!("UID FETCH {} (INTERNALDATE)", uid);
        let (ok, untagged) = self.run_command(&cmd)?;
        if !ok {
            return Ok(None);
        }
        for line in &untagged {
            if let Some(date) = extract_internaldate(line) {
                return Ok(Some(date));
            }
        }
        Ok(None)
    }

    /// 进入 IDLE, 最多等待 `timeout`.收到 EXISTS 推送或超时后发 DONE 返回.
    pub fn idle(&mut self, timeout: Duration) -> Result<IdleResult, ImapError> {
        let tag = self.next_tag();
        let cmd = format!("{} IDLE\r\n", tag);
        self.send_raw(cmd.as_bytes())?;

        // 等待延续响应 `+ ...` 或 tag 完成(不支持 IDLE 时直接返回 BAD).
        loop {
            let raw = self.read_line()?;
            let s = String::from_utf8_lossy(&raw).into_owned();
            if s.starts_with('+') {
                break;
            }
            if s.starts_with(tag.as_str()) {
                // 不支持 IDLE:按超时处理, 交由外层走轮询兜底.
                return Ok(IdleResult::Timeout);
            }
        }

        // 设置读超时为 IDLE 时长:超时触发 DONE.
        self.stream.get_ref().set_read_timeout(Some(timeout))?;

        let mut result = IdleResult::Timeout;
        loop {
            let line = match self.read_line() {
                Ok(l) => l,
                // 读超时:Unix 触发 WouldBlock, Windows 触发 TimedOut(os error 10060).
                // 两者都表示 IDLE 等待期内无推送, 按正常超时处理.
                Err(ImapError::Io(e))
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => {
                    // 试图发 DONE 收尾后向上抛错.
                    let _ = self.send_raw(b"DONE\r\n");
                    return Err(e);
                }
            };
            let s = String::from_utf8_lossy(&line).into_owned();
            if s.starts_with('*') {
                let up = s.to_uppercase();
                if up.contains(" EXISTS") {
                    result = IdleResult::Exists;
                    break;
                }
                // 其它未标记响应(EXPUNGE/OK 等)忽略, 继续等.
                continue;
            }
            if s.starts_with(tag.as_str()) {
                // 服务器主动结束 IDLE.
                break;
            }
        }

        // 发 DONE 并读至 tag 完成.
        self.stream
            .get_ref()
            .set_read_timeout(Some(DEFAULT_TIMEOUT))?;
        self.send_raw(b"DONE\r\n")?;
        loop {
            let raw = self.read_line()?;
            let s = String::from_utf8_lossy(&raw).into_owned();
            if s.starts_with(tag.as_str()) {
                break;
            }
        }
        Ok(result)
    }
}

// ─────────────── 辅助函数 ───────────────

/// 设置 TCP keepalive:连接空闲 10s 后开始探测, 每 10s 一次, 连续 3 次无响应判定断连.
/// 保活 NAT 表项, 避免 IDLE 长连接被中间设备超时断开(10060 错误).
fn set_keepalive(stream: &TcpStream) {
    use socket2::SockRef;
    let sock = SockRef::from(stream);
    let cfg = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(10))
        .with_interval(Duration::from_secs(10));
    let _ = sock.set_tcp_keepalive(&cfg);
}

/// 给 IMAP 参数加双引号(若含空格/特殊字符).授权码/邮箱通常无需, 但保留以防万一.
fn quote(s: &str) -> String {
    if s.chars().all(|c| !c.is_whitespace() && !c.is_control()) && !s.is_empty() {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// 从 FETCH 响应行末解析 `{N}` 字面量长度.
fn parse_literal_len(line: &str) -> Option<usize> {
    let brace_open = line.rfind('{')?;
    let after = &line[brace_open + 1..];
    let brace_close = after.find('}')?;
    after[..brace_close].parse::<usize>().ok()
}

/// 从 `* 1 FETCH (UID 12 INTERNALDATE "17-Jul-2024 12:34:56 +0800")` 提取日期串.
fn extract_internaldate(line: &str) -> Option<String> {
    let key = "INTERNALDATE";
    let pos = line.to_uppercase().find(key)?;
    let after = &line[pos + key.len()..];
    let q1 = after.find('"')?;
    let rest = &after[q1 + 1..];
    let q2 = rest.find('"')?;
    Some(rest[..q2].to_string())
}
