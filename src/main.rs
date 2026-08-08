#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod event_loop;
mod keyboard;
mod mail;
mod notification;
mod server;
mod tray;
mod windows_console;
mod windows_registry;

use server::{Args, SmsMessage};
use std::sync::mpsc;

#[cfg(windows)]
fn log_timestamp() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    unsafe {
        let mut st = std::mem::MaybeUninit::<SYSTEMTIME>::zeroed();
        GetLocalTime(st.as_mut_ptr());
        let st = st.assume_init();
        format!(
            "{:02}/{:02} {:02}:{:02}:{:02}",
            st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
}

#[cfg(unix)]
fn log_timestamp() -> String {
    use libc::{localtime, time, tm};

    unsafe {
        let mut raw_time: libc::time_t = 0;
        time(&mut raw_time);
        let tm_ptr = localtime(&raw_time);
        if tm_ptr.is_null() {
            return "1970-01-01 00:00:00".to_string();
        }
        let tm: tm = *tm_ptr;
        // tm_mon is 0..=11, tm_mday is 1..=31
        format!(
            "{:02}/{:02} {:02}:{:02}:{:02}",
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

#[cfg(not(any(windows, unix)))]
fn log_timestamp() -> String {
    "1970-01-01 00:00:00".to_string()
}

/// 主线程事件循环的用户事件.
/// `pub(crate)` 是因为 `tray` 模块需要构造 `AppEvent::TrayIconEvent` /
/// `AppEvent::MenuEvent` 才能投递给 ui_tx, 而 `event_loop::run` 是泛型
/// 不需要看到这个类型.
pub(crate) enum AppEvent {
    ShowNotification(String, String),
    TrayIconEvent(tray_icon::TrayIconEvent),
    MenuEvent(tray_icon::menu::MenuEvent),
}

/// 极简 `tracing::Subscriber`:把每个事件格式化为一行(首行带时间戳与级别,
/// 多行 continuation 不带), 写入内存环形缓冲并实时广播给 WebSocket 订阅者.
/// 用来自实现, 替换 tracing-subscriber 的 fmt 层, 缩减二进制体积.
struct LogSubscriber {
    lines: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
}

/// 从事件中取出 `message` 字段的文本(其余字段忽略).
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // fmt::Arguments 的 Debug 实现输出即最终文本(无多余引号).
            self.message = format!("{:?}", value);
        }
    }
}

/// 把一条已格式化(含时间戳/级别)的日志行写入环形缓冲并广播.
fn store_log_line(
    lines: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    stamped: &str,
) {
    let mut buf = lines.lock().unwrap();
    buf.push_back(stamped.to_string());
    if buf.len() > 500 {
        buf.pop_front();
    }
    drop(buf);
    crate::server::broadcast_log_line(stamped);
}

impl tracing::Subscriber for LogSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = MessageVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARN",
            tracing::Level::INFO => "INFO",
            tracing::Level::DEBUG => "DEBUG",
            tracing::Level::TRACE => "TRACE",
        };
        // 多行事件合并为一条:仅首行加时间戳与级别(与旧 fmt 行为一致),
        // 内部 \n 保留, 前端转 <br>.
        let mut first = true;
        let mut out = String::new();
        for line in visitor.message.split('\n') {
            if first {
                out.push_str(&format!("{} {} {}", log_timestamp(), level, line));
                first = false;
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        let out = out.trim_end_matches('\n').to_string();
        if !out.is_empty() {
            store_log_line(&self.lines, &out);
        }
    }
}

fn main() {
    // 主线程初始化: 创建唤醒事件 + 初始化 COM STA, 必须在派发任何 UI
    // 事件之前完成. 该函数是 no-op 时机敏感的, 见 event_loop.rs.
    event_loop::install();

    // Register the portable executable's Windows toast identity before any UI
    // is created. This also creates/updates its per-user Start Menu shortcut.
    windows_registry::configure();

    // Parse CLI. With `windows_subsystem = "windows"` there is no console,
    // so --help / usage errors go through AttachConsole (see server::print_out).
    let args = Args::parse();

    // Setup logging: 自实现极简 tracing::Subscriber(见上方 LogSubscriber),
    // 替代 tracing-subscriber 的 fmt 层以减小二进制体积. 日志写入内存环形
    // 缓冲, 由 HTTP /logs 页面读取并实时广播给 WebSocket 订阅者.
    let log_lines = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    server::set_log_buffer(log_lines.clone());
    tracing::subscriber::set_global_default(LogSubscriber {
        lines: log_lines.clone(),
    })
    .expect("failed to install log subscriber");

    // Bounded channel so a slow receiver cannot accumulate an unbounded queue.
    let (sms_tx, sms_rx) = mpsc::sync_channel::<SmsMessage>(100);

    // Spawn HTTP server (blocking accept loop on its own thread).
    let http_port = args.port;
    let http_tx = sms_tx.clone();
    std::thread::spawn(move || {
        server::start_http_server(http_port, http_tx);
    });

    // 启动邮件监听线程.
    // 启用条件(任一满足即可;否则完全不启动, 避免额外线程开销):
    //   1) exe 同目录存在 config.yaml
    //   2) 通过 --config / -c 显式指定了配置路径
    let mail_tx = sms_tx.clone();
    let mail_path = args.config_path.clone();
    std::thread::spawn(move || {
        mail::start(mail_tx, mail_path.as_deref().map(std::path::Path::new));
    });

    // UI 事件通道: SMS/邮件/托盘回调都先投到 ui_tx, 主循环在 event_loop::run
    // 内每次循环顶部排空 ui_rx, 然后再处理平台消息. 排空后再 SetEvent 唤醒
    // (Windows); macOS 走轮询, wakeup 是空操作.
    let (ui_tx, ui_rx) = mpsc::sync_channel::<AppEvent>(100);

    // 构造托盘 (独立于自实现 event_loop). 必须持有返回值到进程退出.
    let _tray_icon = tray::build(ui_tx.clone()).expect("failed to build tray icon");

    // 单一后台线程: 负责 ?code= / ?sms= 解析, 跑键盘 copy+paste, 然后通过
    // ui_tx 通知主线程弹 toast.
    let ui_tx_for_sms = ui_tx.clone();
    std::thread::spawn(move || {
        while let Ok(msg) = sms_rx.recv() {
            match msg {
                SmsMessage::Code(code) => {
                    keyboard::copy_paste_submit(&code, true);
                    let _ = ui_tx_for_sms
                        .send(AppEvent::ShowNotification("收到验证码".to_string(), code));
                }
                SmsMessage::Sms(raw) => {
                    if raw.trim().is_empty() {
                        continue;
                    }
                    match SmsMessage::extract_code(&raw) {
                        Some((title, code)) => {
                            keyboard::copy_paste_submit(&code, true);
                            let notification_body = format!("【{}】: {}", title, code);
                            let _ = ui_tx_for_sms
                                .send(AppEvent::ShowNotification(title, notification_body));
                        }
                        None => {
                            tracing::warn!("Received via ?sms= (unrecognized format): {}", raw);
                        }
                    }
                }
                // 统一通知:auto_paste 时先 copy+paste, submit 控制是否回车;再弹窗.
                SmsMessage::Notify {
                    title,
                    body,
                    auto_paste,
                    submit,
                } => {
                    if auto_paste {
                        keyboard::copy_paste_submit(&body, submit);
                    }
                    let _ = ui_tx_for_sms.send(AppEvent::ShowNotification(title, body));
                }
            }
            // 唤醒主循环 (Windows: SetEvent; macOS: 空操作).
            event_loop::wakeup();
        }
    });

    // 预创建 tray-icon 事件比较用的 MenuId, 避免每次匹配都重新分配字符串.
    let log_menu_id = tray_icon::menu::MenuId::new(tray::LOG_MENU_ID);
    let exit_menu_id = tray_icon::menu::MenuId::new(tray::EXIT_MENU_ID);

    // 移交主线程给平台事件循环. 永不返回 (Exit 菜单走 std::process::exit).
    event_loop::run(ui_rx, move |event| match event {
        AppEvent::ShowNotification(title, body) => {
            // Show the toast on the main thread. The toast's click
            // handler (copy body to clipboard) is an in-process COM
            // event delivered to the apartment that created the toast;
            // the main thread keeps that apartment alive and pumps
            // messages, so the callback is reliably invoked. On a
            // short-lived background thread the callback would never
            // be delivered.
            notification::notify_sms(&title, &body);
        }
        AppEvent::TrayIconEvent(event) => {
            // 托盘图标左键 -> 以 Chrome/Edge 应用模式打开日志页面
            // 只响应 button_state::Down, 避免 Down+Up 重复触发导致双窗口
            if let tray_icon::TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Down,
                ..
            } = event
            {
                server::open_logs_in_app(http_port);
            }
        }
        AppEvent::MenuEvent(event) => {
            if event.id == log_menu_id {
                server::open_logs_in_browser(http_port);
            } else if event.id == exit_menu_id {
                std::process::exit(0);
            }
        }
    });
}
