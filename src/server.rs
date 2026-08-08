use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use tungstenite::handshake::server::{ErrorResponse, Request as WsRequest, Response as WsResponse};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

type LogBuffer = Arc<Mutex<VecDeque<String>>>;

/// WebSocket 日志推送独立端口，绑定规则见 [start_http_server]。
/// 通过 `window.__WS_PORT__` 暴露给前端。
static WS_PORT: OnceLock<u16> = OnceLock::new();

/// A WebSocket subscriber entry, pairing a bounded sync-channel sender with
/// an `Arc<()>` identity token so we can cheaply and deterministically
/// remove the entry from the subscriber list when the WS thread exits (
/// `SyncSender` itself has no comparable identity). Cloning `WsEntry`
/// clones the identity (identical `Arc::ptr_eq`) and the channel (points
/// at the same inner channel).
#[derive(Clone)]
struct WsEntry {
    id: Arc<()>,
    tx: SyncSender<String>,
}

impl PartialEq for WsEntry {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.id, &other.id)
    }
}

impl Eq for WsEntry {}

/// Global handle to the in-memory log ring buffer, shared with the HTTP
/// server so the `/logs` endpoint can render it.
static LOG_BUFFER: OnceLock<LogBuffer> = OnceLock::new();

/// List of active WebSocket subscriber sender-ends. Each new `/logs/ws`
/// connection registers a SyncSender here; every time a new log line is
/// produced we fan-out a copy to each sender. When the WS thread detects a
/// send-error or closes the socket it removes its own entry from the list.
static WS_SUBSCRIBERS: OnceLock<Mutex<Vec<WsEntry>>> = OnceLock::new();

fn ws_subscribers() -> &'static Mutex<Vec<WsEntry>> {
    WS_SUBSCRIBERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Install the shared log buffer reference. Called once in `main()` before
/// the HTTP server starts.
pub fn set_log_buffer(buf: LogBuffer) {
    let _ = LOG_BUFFER.set(buf);
}

/// Snapshot of the current log lines (oldest first).
fn snapshot_logs() -> Vec<String> {
    match LOG_BUFFER.get() {
        Some(buf) => buf.lock().unwrap().iter().cloned().collect(),
        None => Vec::new(),
    }
}

/// Push a freshly stamped log line to every live WebSocket subscriber.
/// Called from the logging writer path right after the line is stored into
/// the ring buffer. A send-failure simply drops that sender from the list;
/// the corresponding reader thread will clean up when it notices.
pub fn broadcast_log_line(line: &str) {
    // 没有订阅者时直接返回, 省去无谓的锁竞争与字符串分配.
    // 浏览器没打开 /logs 时每条日志都会走到这里, 早期退出收益明显.
    if ws_subscribers().lock().unwrap().is_empty() {
        return;
    }
    // 日志内部 \n 在推送到前端时转为 <br>, 保证 insertAdjacentHTML 正确渲染换行
    let line = line.replace('\n', "<br>");
    let mut subs = ws_subscribers().lock().unwrap();
    let mut idx = 0;
    while idx < subs.len() {
        if subs[idx].tx.try_send(line.to_string()).is_err() {
            subs.swap_remove(idx);
        } else {
            idx += 1;
        }
    }
}

/// Print to the parent console (Windows GUI subsystem) or stdout (other).
pub fn print_out(msg: &str) {
    crate::windows_console::print_to_console(msg, false);
}

/// Print an error to the parent console (Windows) or stderr (other).
pub fn print_err(msg: &str) {
    crate::windows_console::print_to_console(msg, true);
}

/// Messages from the HTTP server (or mail listener) to the background handler
/// thread.
pub enum SmsMessage {
    Code(String),
    Sms(String),
    /// Unified notification: title/body are popup content, body is also the
    /// payload copied to clipboard when the notification is clicked. When
    /// auto_paste=true the main thread additionally copy+pastes body into the
    /// focused window; submit controls whether Enter is pressed after the
    /// paste (only meaningful when auto_paste=true).
    Notify {
        title: String,
        body: String,
        auto_paste: bool,
        submit: bool,
    },
}

impl SmsMessage {
    /// Parse a raw SMS string in the format "【Company】...1234"
    /// and extract (company_name, verification_code).
    ///
    /// Returns `None` if the string doesn't match the expected format.
    pub fn extract_code(raw: &str) -> Option<(String, String)> {
        let start = raw.find('【')?;
        let after_bracket = &raw[start + '【'.len_utf8()..];
        let end = after_bracket.find('】')?;
        let company = after_bracket[..end].trim().to_string();

        let after_company = &after_bracket[end + '】'.len_utf8()..];
        let code = extract_digits(after_company)?;

        Some((company, code))
    }
}

/// Find the longest sequence of 4-8 consecutive ASCII digits in the string.
/// Ties go to the first match.
fn extract_digits(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut best_start = None;
    let mut best_len = 0;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let len = i - start;
            if len >= 4 && len <= 8 && len > best_len {
                best_start = Some(start);
                best_len = len;
            }
        } else {
            i += 1;
        }
    }

    best_start.map(|start| {
        bytes[start..start + best_len]
            .iter()
            .map(|&b| b as char)
            .collect()
    })
}

#[derive(Debug)]
pub struct Args {
    pub port: u16,
    pub config_path: Option<String>,
}

impl Args {
    pub fn parse() -> Self {
        let mut port = 18080;
        let mut config_path: Option<String> = None;
        let mut args = std::env::args().skip(1);

        while let Some(arg) = args.next() {
            let value = match arg.as_str() {
                "-p" | "--port" => args
                    .next()
                    .unwrap_or_else(|| usage_error("Missing value for --port")),
                "-c" | "--config" => {
                    let v = args
                        .next()
                        .unwrap_or_else(|| usage_error("Missing value for --config"));
                    config_path = Some(v);
                    continue;
                }
                "-h" | "--help" => {
                    print_out(
                        "SMS Notifier\n\nUsage: sms-notifier [--port <PORT>] [--config <PATH>]\n",
                    );
                    std::process::exit(0);
                }
                _ => {
                    if let Some(rest) = arg.strip_prefix("--port=") {
                        rest.to_owned()
                    } else if let Some(rest) = arg.strip_prefix("--config=") {
                        config_path = Some(rest.to_owned());
                        continue;
                    } else {
                        usage_error(&format!("Unknown argument: {arg}"))
                    }
                }
            };
            port = value
                .parse()
                .unwrap_or_else(|_| usage_error(&format!("Invalid port: {value}")));
        }

        Self { port, config_path }
    }
}

fn usage_error(message: &str) -> ! {
    print_err(&format!(
        "{message}\nUsage: sms-notifier [--port <PORT>] [--config <PATH>]\n"
    ));
    std::process::exit(2)
}

fn get_local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:8").ok()?;
    let addr = socket.local_addr().ok()?;
    match addr.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

/// Open the default web browser at `http://127.0.0.1:<port>/logs`.
/// Uses platform-specific shell commands so no extra dependency is required.
pub fn open_logs_in_browser(port: u16) {
    let url = format!("http://127.0.0.1:{}/logs", port);
    let result = open_url(&url);
    if let Err(e) = result {
        error!("Failed to open browser to {}: {:?}", url, e);
    }
}

/// 以 Chrome/Edge 应用模式打开日志页面（无地址栏工具栏，居中显示）。
/// 使用 `cmd /c start` 调用，与用户命令行 `start chrome --app=...` 行为一致，
/// 通过 Shell 查找可执行文件（支持 App Paths 注册表），避免 PATH 未包含的问题。
/// 仅 Windows 实现；其他平台回退到 `open_logs_in_browser`。
#[cfg(target_os = "windows")]
pub fn open_logs_in_app(port: u16) {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics;

    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    // 与 log_viewer.html 的 <title> 保持一致，用于查找已存在的日志窗口。
    const LOG_WINDOW_TITLE: &str = "SMS Notifier Logs";

    // 若日志窗口已打开（用户可能只是再次点击了托盘图标），直接将其提到前台，
    // 避免每次点击都启动一个新的浏览器进程/窗口。
    if focus_existing_log_window(LOG_WINDOW_TITLE) {
        return;
    }

    let window_width = 1000;
    let window_height = 700;

    let (pos_x, pos_y) = unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        ((sw - window_width) / 2, (sh - window_height) / 2)
    };

    let url = format!("http://127.0.0.1:{}/logs", port);
    let temp_dir = std::env::temp_dir().join("log_viewer_profile");
    let user_data_arg = format!("--user-data-dir={}", temp_dir.to_string_lossy());

    let base_args = [
        &format!("--app={}", url),
        &format!("--window-size={},{}", window_width, window_height),
        &format!("--window-position={},{}", pos_x, pos_y),
        &user_data_arg,
        "--disable-features=Translate,OptimizationHints,MediaRouter,AutofillServerCommunication,AutofillCreditCardAuthentication",
        "--enable-features=OverlayScrollbar",
        "--disk-cache-size=10240",
        "--media-cache-size=10240",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-metrics-reporting",
        "--disable-default-apps",
        "--disable-extensions",
        "--disable-translate",
        "--disable-metrics",
        "--disable-plugins",
        "--disable-sync",
        "--disable-gpu",
        "--dns-prefetch-disable",
        "--process-per-site",
        "--force-dark-mode",
        "--base-background-color=0xFF1E1E1E",
        "--no-default-browser-check",
        "--no-first-run",
    ];

    /// 通过 `cmd /c start <browser>` 启动浏览器，复用 base_args 中的启动参数。
    fn try_start(browser: &str, args: &[&str]) -> bool {
        const CMD_PREFIX: [&str; 3] = ["/c", "start", ""];
        let mut cmd = Command::new("cmd");
        cmd.args(&CMD_PREFIX)
            .arg(browser)
            .args(args)
            .creation_flags(CREATE_NO_WINDOW);
        cmd.spawn().is_ok()
    }

    // 1. 优先尝试启动 Chrome
    if try_start("chrome", &base_args) {
        return;
    }
    // 2. 没有 Chrome 则尝试启动 Edge
    if try_start("msedge", &base_args) {
        return;
    }
    // 3. 都失败则回退到系统默认浏览器
    error!("Neither Chrome nor Edge found; falling back to default browser");
    open_logs_in_browser(port);
}

/// 枚举当前可见窗口，若某个窗口标题包含 `title` 则将其恢复并提到前台，
/// 返回是否找到了已存在的窗口。用于避免重复打开日志窗口。
#[cfg(target_os = "windows")]
fn focus_existing_log_window(title: &str) -> bool {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    struct FindCtx {
        title: String,
        found: HWND,
    }

    // WNDENUMPROC 返回 BOOL（即 i32）：返回非 0 继续枚举，0 停止。
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
        let ctx = &mut *(lparam as *mut FindCtx);
        // 仅匹配可见窗口，跳过最小化到任务栏/托盘的隐藏窗口。
        if IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if len > 0 {
            let text = String::from_utf16_lossy(&buf[..len as usize]);
            if text.contains(ctx.title.as_str()) {
                ctx.found = hwnd;
                return 0; // 找到后停止枚举
            }
        }
        1
    }

    let mut ctx = FindCtx {
        title: title.to_string(),
        found: std::ptr::null_mut(),
    };
    unsafe {
        EnumWindows(Some(enum_proc), &mut ctx as *mut _ as LPARAM);
        if !ctx.found.is_null() {
            ShowWindow(ctx.found, SW_RESTORE);
            SetForegroundWindow(ctx.found);
            return true;
        }
    }
    false
}

#[cfg(not(target_os = "windows"))]
pub fn open_logs_in_app(port: u16) {
    open_logs_in_browser(port);
}

#[cfg(target_os = "windows")]
fn open_url(url: &str) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    // CREATE_NO_WINDOW (0x08000000) prevents cmd.exe from briefly flashing
    // a console window when it is spawned. Our parent process uses
    // `windows_subsystem = "windows"` but cmd.exe itself is a console
    // subsystem binary, so Windows would normally allocate a console for
    // it — this flag suppresses that allocation.
    //
    // `cmd /c start "" URL` then invokes ShellExecute on the URL, opening
    // the default browser. The empty string before URL is required because
    // `start` treats the first quoted argument as a window title.
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    Command::new("cmd")
        .args(["/c", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

#[cfg(target_os = "macos")]
fn open_url(url: &str) -> std::io::Result<()> {
    use std::process::Command;
    Command::new("open").arg(url).spawn().map(|_| ())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn open_url(url: &str) -> std::io::Result<()> {
    use std::process::Command;
    Command::new("xdg-open").arg(url).spawn().map(|_| ())
}

// ---------------------------------------------------------------------------
// WebSocket 日志推送：走独立 WS 端口（HTTP 主端口 + 1）。
//
// 与 HTTP 服务分离是为了职责清晰：HTTP 侧是极简手写实现，不处理 101
// Upgrade；WS 侧直接在 `spawn_ws_listener` 里为 loopback 绑定第二个
// TcpListener（port + 1），用 tungstenite::accept_hdr_with_config 完成
// RFC 6455 握手，之后由 run_websocket_connection 做后续 framed 读写。
// 前端拿到的日志 HTML 里会嵌入 window.__WS_PORT__ 变量，JS 据此
// 拼出 ws://location.hostname:ws_port 的实时推送地址。
// ---------------------------------------------------------------------------

fn spawn_ws_listener(bind_addr: (Ipv4Addr, u16)) {
    let listener = match TcpListener::bind(bind_addr) {
        Ok(l) => l,
        Err(e) => {
            warn!(
                "failed to bind WebSocket log listener on {}:{}: {}",
                bind_addr.0, bind_addr.1, e
            );
            return;
        }
    };
    let _ = listener.set_nonblocking(false);

    thread::Builder::new()
        .name("logs-ws-accept".to_string())
        .spawn(move || {
            for stream_res in listener.incoming() {
                let stream = match stream_res {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("WebSocket accept error: {:?}", e);
                        continue;
                    }
                };
                let is_loopback = stream
                    .peer_addr()
                    .map(|a| a.ip().is_loopback())
                    .unwrap_or(false);
                if !is_loopback {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                }
                stream.set_read_timeout(Some(Duration::from_millis(200))).ok();
                stream.set_nonblocking(false).ok();

                let ws_result = tungstenite::accept_hdr_with_config(
                    stream,
                    |req: &WsRequest, resp: WsResponse| -> Result<WsResponse, ErrorResponse> {
                        let _ = req;
                        // tungstenite 已自动写入 Upgrade/Connection/
                        // Sec-WebSocket-Accept，此处不需要加自定义头。
                        Ok(resp)
                    },
                    Some(WebSocketConfig::default()),
                );
                let mut ws = match ws_result {
                    Ok(w) => w,
                    Err(e) => {
                        warn!("WebSocket handshake failed: {:?}", e);
                        continue;
                    }
                };

                // 新连接先发送全量缓存日志，再注册到订阅列表
                // \n 转为 <br> 与 broadcast_log_line 保持一致
                let snapshot = snapshot_logs();
                let mut snap_ok = true;
                for line in &snapshot {
                    let html_line = line.replace('\n', "<br>");
                    if ws.send(Message::Text(html_line.into())).is_err() {
                        snap_ok = false;
                        break;
                    }
                }
                if !snap_ok {
                    let _ = ws.close(None);
                    continue;
                }
                let _ = ws.flush();

                let (tx, rx): (SyncSender<String>, Receiver<String>) = sync_channel(1024);
                let entry = WsEntry {
                    id: Arc::new(()),
                    tx,
                };
                ws_subscribers().lock().unwrap().push(entry.clone());

                thread::Builder::new()
                    .name("logs-ws".to_string())
                    .spawn(move || {
                        run_websocket_connection(ws, rx, entry);
                    })
                    .ok();
            }
        })
        .ok();
}

/// 服务端主动心跳间隔. 浏览器收到 Ping 会按 RFC 自动回 Pong.
const WS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

fn run_websocket_connection<S>(mut ws: WebSocket<S>, rx: Receiver<String>, entry: WsEntry)
where
    S: std::io::Read + std::io::Write,
{
    let mut last_ping = Instant::now();
    let mut awaiting_pong = false;

    loop {
        let mut batched = 0usize;
        while let Ok(line) = rx.try_recv() {
            if ws.send(Message::Text(line.into())).is_err() {
                unregister_subscriber(&entry);
                return;
            }
            batched += 1;
            if batched > 64 {
                break;
            }
        }
        if batched > 0 {
            if ws.flush().is_err() {
                unregister_subscriber(&entry);
                return;
            }
        }

        // 心跳保活: 异常断开(合盖/网络瞬断)不会发 Close, 服务端会一直卡在
        // ws.read() 的超时里空转, 导致连接线程与订阅槽泄漏. 超过一个间隔仍未
        // 收到上次 Ping 的 Pong 即判定对端已死并清理.
        let now = Instant::now();
        if now.duration_since(last_ping) >= WS_HEARTBEAT_INTERVAL {
            if awaiting_pong {
                break;
            }
            if ws.send(Message::Ping(Vec::new().into())).is_err() || ws.flush().is_err() {
                break;
            }
            last_ping = now;
            awaiting_pong = true;
        }

        match ws.read() {
            Ok(msg) => match msg {
                Message::Text(_) | Message::Binary(_) => {
                    let _ = ws.send(Message::Text("ok".into()));
                }
                Message::Ping(payload) => {
                    let _ = ws.send(Message::Pong(payload));
                }
                Message::Pong(_) => {
                    awaiting_pong = false;
                }
                Message::Close(_) => {
                    let _ = ws.close(None);
                    break;
                }
                Message::Frame(_) => unreachable!(),
            },
            Err(tungstenite::Error::Io(ref io))
                if io.kind() == std::io::ErrorKind::WouldBlock
                    || io.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => {
                break;
            }
        }
    }

    unregister_subscriber(&entry);
}

fn unregister_subscriber(entry: &WsEntry) {
    let mut subs = ws_subscribers().lock().unwrap();
    if let Some(pos) = subs.iter().position(|s| s == entry) {
        subs.swap_remove(pos);
    }
}

/// Escape a string for safe embedding inside a JSON string literal.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Build a JSON payload from the log buffer for `/logs/raw`.
fn render_logs_json() -> String {
    let lines = snapshot_logs();
    let mut json = String::from("{\"count\":");
    json.push_str(&lines.len().to_string());
    json.push_str(",\"lines\":[");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push('"');
        json.push_str(&json_escape(line));
        json.push('"');
    }
    json.push_str("]}");
    json
}

/// The log viewer HTML is authored as a standalone file
/// `assets/log_viewer.html` (so it can be edited / previewed directly in a
/// browser). We embed it verbatim at compile time with `include_str!` so
/// the final binary stays self-contained and there is no runtime file IO
/// or risk of drift between server.rs and the HTML source-of-truth.
///
/// Previewing: open `./assets/log_viewer.html` directly in a browser. It
/// ships with a small demo-log injector (clearly marked, optional to
/// remove) that seeds 8 sample historical lines + appends a new line
/// every 1.2s for tweaking the styles without running the Rust server.
fn render_logs_html_with_port(ws_port: u16) -> String {
    let payload = include_str!("../assets/log_viewer.html");
    let injected = format!(
        "<script>window.__WS_PORT__ = {};</script>\n",
        ws_port
    );
    // 查找 <head> 标签，在其后插入 <script>
    if let Some(pos) = payload.find("<head>") {
        let insert_at = pos + "<head>".len();
        let mut out = String::with_capacity(payload.len() + injected.len() + 1);
        out.push_str(&payload[..insert_at]);
        out.push_str("\n    ");
        out.push_str(&injected);
        out.push_str(&payload[insert_at..]);
        out
    } else {
        // fallback: 直接追加到最前面
        let mut out = String::with_capacity(payload.len() + injected.len() + 2);
        out.push_str(&injected);
        out.push('\n');
        out.push_str(payload);
        out
    }
}

// Deprecated: 保留一份无 port 注入的版本，方便离线双击 log_viewer.html
// 调试样式时通过 include_str 拿到原文；运行时 /logs 路由始终用
// render_logs_html_with_port。
#[allow(dead_code)]
fn render_logs_html() -> &'static str {
    include_str!("../assets/log_viewer.html")
}

/// Run the HTTP server until the process exits. Accepts connections on the
/// calling thread and hands each one to a dedicated worker thread so a slow
/// client can't stall the accept loop.
///
/// 这是一个极简的手写 HTTP/1.1 实现（不再依赖 `tiny_http`），因为我们只
/// 服务少数几个简单的 GET 端点；完整框架会无谓增大二进制。WebSocket 实时
/// 日志走独立端口，由 `spawn_ws_listener` + tungstenite 处理，与本服务互不
/// 干扰。
pub fn start_http_server(port: u16, tx: SyncSender<SmsMessage>) {
    // WebSocket 日志推送走独立端口（HTTP 主端口 + 1，只监听 loopback）。
    // 该端口直接由 tungstenite::accept_hdr_with_config 完成 RFC 6455 握手，
    // 与下面的 HTTP 服务互不干扰。
    let ws_port = port.wrapping_add(1);
    let _ = WS_PORT.set(ws_port);
    spawn_ws_listener((Ipv4Addr::LOCALHOST, ws_port));
    info!("Logs WebSocket on ws://127.0.0.1:{ws_port} (localhost only)");

    let display_ip = get_local_ipv4()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let server = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("Failed to bind HTTP server on port {}: {}", port, e);
            crate::windows_console::show_message_box(&msg, true);
            std::process::exit(1);
        }
    };
    info!(
        "Server listening on http://{}:{} (LAN) | http://127.0.0.1:{} (local)",
        display_ip, port, port
    );
    info!("Logs available at http://127.0.0.1:{}/logs", port);

    for stream_res in server.incoming() {
        let stream = match stream_res {
            Ok(s) => s,
            Err(e) => {
                warn!("HTTP accept error: {:?}", e);
                continue;
            }
        };
        let tx = tx.clone();
        thread::Builder::new()
            .name("http-client".to_string())
            .spawn(move || handle_http_client(stream, ws_port, tx))
            .ok();
    }
}

/// 处理单个 HTTP 连接：读取一条请求并应答，随后关闭连接。
/// 支持 GET 与 POST（参数均来自 URL，请求体忽略）；其余方法返回 405；日志查看接口限定 localhost。
fn handle_http_client(mut stream: TcpStream, ws_port: u16, tx: SyncSender<SmsMessage>) {
    let is_local = stream
        .peer_addr()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(false);

    // 用 BufReader 读取请求行与请求头；应答仍写回原始 stream。
    let clone = match stream.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut reader = BufReader::new(clone);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let request_line = request_line.trim_end();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let full_path = parts.next().unwrap_or("/");

    // 读取并丢弃请求头（遇到空行结束），顺便记录 Content-Length 以防御性消费请求体。
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(rest) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        let _ = reader.read_exact(&mut buf);
    }

    let (path, query) = full_path.split_once('?').unwrap_or((full_path, ""));

    let (status, content_type, body) = route_http(method, path, query, is_local, ws_port, &tx);
    write_http_response(&mut stream, status, content_type, &body);
}

/// 根据方法、路径与查询参数生成 (状态码, Content-Type, 响应体)。
fn route_http(
    method: &str,
    path: &str,
    query: &str,
    is_local: bool,
    ws_port: u16,
    tx: &SyncSender<SmsMessage>,
) -> (u16, &'static str, String) {
    const CT_HTML: &str = "text/html; charset=utf-8";
    const CT_TEXT: &str = "text/plain; charset=utf-8";
    const CT_JSON: &str = "application/json; charset=utf-8";

    if method != "GET" && method != "POST" {
        return (405, CT_TEXT, "Method not allowed\n".to_string());
    }

    // 日志查看接口——限定 localhost 以保护隐私。
    // /?code= 与 /?sms= 仍对 LAN 开放（手机需要访问）。
    if path == "/logs" || path == "/logs/" || path == "/logs/raw" {
        if !is_local {
            return (403, CT_TEXT, "Forbidden\n".to_string());
        }
        if path == "/logs/raw" {
            return (200, CT_JSON, render_logs_json());
        }
        // /logs 或 /logs/ —— HTML 页面。把 ws_port 注入 window.__WS_PORT__，
        // 前端从同一 HTML 拿到独立 WS 端口地址，避免拼 port+1 被防火墙/端口冲突绕开。
        return (200, CT_HTML, render_logs_html_with_port(ws_port));
    }

    if path != "/" {
        return (404, CT_TEXT, "Not found\n".to_string());
    }

    if let Some(code) = query_param(query, "code") {
        // 拒绝空 code，避免粘贴空内容并按 Enter 提交空表单。
        if code.trim().is_empty() {
            return (400, CT_TEXT, "Empty code\n".to_string());
        }
        info!("Received Code: {}", code);
        if tx.send(SmsMessage::Code(code)).is_err() {
            error!("Failed to send code to notification handler");
        }
        return (200, CT_TEXT, "Code received\n".to_string());
    }

    if let Some(sms) = query_param(query, "sms") {
        info!("Received SMS: {}", sms);
        if tx.send(SmsMessage::Sms(sms)).is_err() {
            error!("Failed to send SMS to handler");
        }
        return (200, CT_TEXT, "SMS received\n".to_string());
    }

    let help = "SMS Notifier endpoints:\n\
             - GET/POST /?code=<verification>\n\
             - GET/POST /?sms=<raw sms text>\n\
             - GET /logs          (HTML log viewer)\n";
    (200, CT_TEXT, help.to_string())
}

/// 写出一条最小化的 HTTP/1.1 响应并关闭连接。
fn write_http_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    // body 可能含多字节 UTF-8（中文），Content-Length 必须用字节数。
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        status = status,
        reason = status_text(status),
        content_type = content_type,
        len = body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// 极简状态码 -> 原因短语映射（仅覆盖我们用到的）。
fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    }
}

/// Extract the percent-decoded value of the given query parameter.
/// Duplicate keys: last one wins. Iterates in reverse so only the matching
/// pair is decoded (instead of decoding every duplicate before picking one).
fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&').rev() {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

/// Minimal URL percent-decoding: `%XX` escapes and `+` as space.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push(hi * 16 + lo);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
