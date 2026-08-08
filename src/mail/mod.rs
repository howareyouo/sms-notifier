//! 邮件监听模块入口:加载 config.yaml 并为每个账号启动独立监听线程.
//!
//! 与 Python 版 mail/ 等价:IMAP 监听, 验证码提取, 通知统一接入主项目的
//! `SmsMessage` 通道, 复用现有的 toast 通知与剪贴板/键盘模拟设施.

pub mod config;
pub mod imap_client;
pub mod listener;
pub mod message;

use std::path::Path;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;

use crate::server::SmsMessage;

/// 加载配置并按账号并行启动监听线程.
///
/// - `custom_path = Some(p)`:从 p 读取 config.yaml.
/// - `custom_path = None`:读取默认路径(exe 同目录 config.yaml).
///
/// 仅当解析出有效账号时才启动线程.默认文件不存在且也没传自定义路径时静默返回--
/// 说明用户不启用邮件监听, 避免产生任何开销.
pub fn start(tx: SyncSender<SmsMessage>, custom_path: Option<&Path>) {
    let cfg = config::load_config(custom_path);
    if cfg.accounts.is_empty() {
        if custom_path.is_none() && !config::default_config_file().exists() {
            // 默认文件不存在也没传自定义路径:不启用邮件监听, 保持静默.
            return;
        }
        tracing::info!("No valid mail accounts configured; skipping mail listener.");
        return;
    }
    let keywords_arc: Arc<Vec<String>> = Arc::new(cfg.match_keywords);
    tracing::info!(
        "Loaded {} mail account(s) with {} keyword(s); starting listeners…",
        cfg.accounts.len(),
        keywords_arc.len()
    );
    for acc in cfg.accounts {
        let tx = tx.clone();
        let kw = Arc::clone(&keywords_arc);
        let name = format!("mail-{}", acc.label_or_email());
        if let Err(e) = std::thread::Builder::new()
            .name(name)
            .spawn(move || listener::listen(acc, kw, cfg.notify_unmatched, tx))
        {
            tracing::warn!("Failed to spawn mail listener thread: {}", e);
        }
    }
}
