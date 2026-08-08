//! 单个邮箱的监听循环,带断线重连与动态退避.
//!
//! 重连策略按账号是否支持 IMAP IDLE 区分:
//! - 支持 IDLE(长连接推送):断线后固定短间隔快速重连,不退避--
//!   IDLE 连接是常态,快速重连不会触发服务器限流.
//! - 不支持 IDLE(定时轮询):连接失败时按 `BACKOFF_FACTOR` 倍增退避,
//!   上限 `MAX_POLL_INTERVAL`,降低查询频率避免被限流.
//! - 首次连接前(未知是否支持 IDLE):前 3 次快速重试(1s/2s/5s),
//!   仍失败则按轮询账号退避(保守策略).
//! 连接成功后清零失败计数,重置间隔到基线,并持久化到 config.yaml.

use std::collections::HashSet;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::Duration;

use crate::server::SmsMessage;

use super::config::{
    self, Account, BACKOFF_FACTOR, FALLBACK, MAX_POLL_INTERVAL, STARTUP_RECENT_SECONDS,
};
use super::imap_client::{IdleResult, ImapClient, ImapError};
use super::message::{self, ParsedMessage};

/// IDLE 账号断线后的固定重连间隔(秒).
const IDLE_RECONNECT_SECS: u64 = 5;
/// 首次连接前(未知是否支持 IDLE)的前 3 次快速重试间隔(秒).
const FAST_RETRY_SECS: [u64; 3] = [1, 2, 5];

/// 单个邮箱的监听循环(重连 + 动态退避).
/// `match_keywords` 通过 `Arc` 在所有监听线程间共享.
pub fn listen(
    account: Account,
    match_keywords: Arc<Vec<String>>,
    notify_unmatched: bool,
    tx: SyncSender<SmsMessage>,
) {
    let label = account.label_or_email().to_string();
    let base = account.poll_interval.max(FALLBACK);
    let mut interval = account.last_ok_interval.unwrap_or(base).max(base);
    let mut fail_count: u32 = 0;
    // 上次成功会话是否支持 IDLE;None 表示尚未成功连接过.
    let mut last_supports_idle: Option<bool> = None;

    loop {
        match run_session(
            &account,
            &label,
            &mut interval,
            base,
            &mut fail_count,
            &mut last_supports_idle,
            &match_keywords,
            notify_unmatched,
            &tx,
        ) {
            Ok(()) => break, // 当前实现不会正常退出
            Err(e) => {
                tracing::warn!("[{}] listener error: {}", label, e);
            }
        }

        fail_count += 1;
        let wait = match last_supports_idle {
            // 已知支持 IDLE:断线就是断线,固定快速重连,不退避.
            // IDLE 连接是服务器预期行为,快速重连不会触发限流.
            Some(true) => {
                tracing::warn!(
                    "[{}] IDLE connection lost; reconnecting in {}s",
                    label,
                    IDLE_RECONNECT_SECS
                );
                IDLE_RECONNECT_SECS
            }
            // 已知不支持 IDLE(轮询账号):指数退避,降低查询频率避免被限流.
            Some(false) => {
                backoff(&mut interval);
                tracing::warn!(
                    "[{}] reconnect interval set to {}s; reconnecting in {}s",
                    label,
                    interval,
                    interval
                );
                interval
            }
            // 尚未成功连接过,未知是否支持 IDLE:
            // 前 3 次快速重试应对瞬时网络抖动;仍失败则按轮询账号退避(保守).
            None => {
                if (fail_count as usize) <= FAST_RETRY_SECS.len() {
                    let w = FAST_RETRY_SECS[(fail_count - 1) as usize];
                    tracing::warn!("[{}] fast retry #{} in {}s", label, fail_count, w);
                    w
                } else {
                    // 首次进入退避时从 base 开始倍增
                    if fail_count == FAST_RETRY_SECS.len() as u32 + 1 {
                        interval = base;
                    }
                    backoff(&mut interval);
                    tracing::warn!(
                        "[{}] poll interval adjusted to {}s on error; reconnecting in {}s",
                        label,
                        interval,
                        interval
                    );
                    interval
                }
            }
        };

        std::thread::sleep(Duration::from_secs(wait));
    }
}

/// 完整 IMAP 会话:连接 → 登录 → 首次 SEARCH → 启动时兜底检查 → 监听循环.
/// `last_supports_idle` 在判断 CAPABILITY 后写回,供外层决定重连策略.
fn run_session(
    account: &Account,
    label: &str,
    interval: &mut u64,
    base: u64,
    fail_count: &mut u32,
    last_supports_idle: &mut Option<bool>,
    match_keywords: &[String],
    notify_unmatched: bool,
    tx: &SyncSender<SmsMessage>,
) -> Result<(), ImapError> {
    let mut client = ImapClient::connect(
        &account.server,
        account.port,
        &account.email,
        &account.password,
    )?;
    client.send_id();
    client.select_inbox()?;

    // 连接成功:清零失败计数并重置间隔到基线.一次成功即视为网络已恢复,
    // 不应让上一轮失败累积的高间隔继续影响本次 IDLE/轮询周期.
    *fail_count = 0;
    if *interval != base {
        *interval = base;
        config::save_account_interval(&account.email, base);
    }

    let uids = client.uid_search_all()?;
    let mut seen: HashSet<u32> = uids.iter().copied().collect();
    let supports_idle = client.supports_idle();
    *last_supports_idle = Some(supports_idle);
    if supports_idle {
        tracing::info!("[{}] connected; IDLE push enabled", label);
    } else {
        tracing::info!(
            "[{}] connected; IDLE unsupported, poll interval {}s",
            label,
            *interval
        );
    }

    // 启动兜底检查:检查最近一封邮件,避免刚启动前那段时间漏掉验证码.
    check_latest(&mut client, &uids, account, match_keywords, tx)?;

    loop {
        if supports_idle {
            match client.idle(Duration::from_secs(*interval))? {
                IdleResult::Exists => {
                    tracing::info!("[{}] server push received; checking mailbox", label);
                }
                IdleResult::Timeout => {}
            }
        } else {
            std::thread::sleep(Duration::from_secs(*interval));
        }
        check_new(
            &mut client,
            &mut seen,
            account,
            match_keywords,
            notify_unmatched,
            tx,
        )?;
    }
}

/// 对比当前 UID 集合与 `seen`,对新邮件发通知.
/// UID 单调递增,删除后不会被复用.
fn check_new(
    client: &mut ImapClient,
    seen: &mut HashSet<u32>,
    account: &Account,
    match_keywords: &[String],
    notify_unmatched: bool,
    tx: &SyncSender<SmsMessage>,
) -> Result<(), ImapError> {
    let current = client.uid_search_all()?;
    let mut new_uids: Vec<u32> = current
        .iter()
        .copied()
        .filter(|u| !seen.contains(u))
        .collect();
    new_uids.sort_unstable();
    for uid in new_uids {
        if let Ok(Some(raw)) = client.uid_fetch_rfc822(uid) {
            let parsed = message::parse(&raw);
            notify_message(&parsed, account, notify_unmatched, match_keywords, tx);
        }
        seen.insert(uid);
    }
    // 清理邮箱里已经不存在的 UID,避免 seen 无限增长.
    seen.retain(|u| current.contains(u));
    Ok(())
}

/// 启动兜底检查:取最近一封邮件,先用 INTERNALDATE 判断是否在新鲜度窗口内;
/// 陈旧就跳过;在窗口内再下载完整正文做验证码提取(仅命中时才弹通知).
fn check_latest(
    client: &mut ImapClient,
    uids: &[u32],
    account: &Account,
    match_keywords: &[String],
    tx: &SyncSender<SmsMessage>,
) -> Result<(), ImapError> {
    let latest = match uids.last() {
        Some(u) => *u,
        None => return Ok(()),
    };
    if let Ok(Some(date_str)) = client.uid_fetch_internaldate(latest) {
        if let Some(age) = parse_internaldate_age_secs(&date_str) {
            if age > STARTUP_RECENT_SECONDS as i64 {
                return Ok(()); // 大概率已经过期,跳过
            }
        }
    }
    if let Ok(Some(raw)) = client.uid_fetch_rfc822(latest) {
        let parsed = message::parse(&raw);
        notify_message(&parsed, account, false, match_keywords, tx);
    }
    Ok(())
}

/// 解析完成的邮件后处理:打日志,可选自动粘贴验证码,弹系统通知.
/// `show_always=false` 表示"只在提取到验证码时才弹通知"--用于启动兜底检查.
fn notify_message(
    parsed: &ParsedMessage,
    account: &Account,
    show_always: bool,
    match_keywords: &[String],
    tx: &SyncSender<SmsMessage>,
) {
    let subject = if parsed.subject.is_empty() {
        "验证码请查收".to_string()
    } else {
        parsed.subject.clone()
    };
    let label = account.label_or_email().to_string();
    let combined = format!("{}\n{}\n{}", subject, parsed.from, parsed.body);
    let code = if message::has_match_keyword(&combined, match_keywords) {
        message::extract_code(&combined)
    } else {
        None
    };

    // 构建单条日志，内部换行用 \n，tracing 事件合并为一条推送给前端，
    // broadcast_log_line 将 \n 转为 <br> 推送到前端
    let mut log_msg = format!("📨 [{}] 新邮件: [{} | {}] :", label, subject, parsed.from);
    if !parsed.body.is_empty() {
        log_msg.push_str(&format!("\n{}", parsed.body));
    }
    if let Some(ref c) = code {
        log_msg.push_str(&format!("\n🔑 验证码已复制: {}", c));
    }
    tracing::info!("{}", log_msg);

    if let Some(c) = code {
        // 命中验证码:auto_paste=true, submit=false.
        // 与 Python 行为一致--邮件到达时焦点窗口未知,不贸然按回车
        // (否则可能误提交半写的回复或命令)
        let _ = tx.send(SmsMessage::Notify {
            title: format!("📧 新邮件 ({})｜{}", label, subject),
            body: c,
            auto_paste: true,
            submit: false,
        });
    } else if show_always {
        // 没命中验证码但 show_always 打开:发一条普通通知, body 为发件人
        // (点击通知后 body 仍会复制到剪贴板)
        let _ = tx.send(SmsMessage::Notify {
            title: format!("📧 新邮件 ({})｜{}", label, subject),
            body: parsed.from.clone(),
            auto_paste: false,
            submit: false,
        });
    }
}

/// 出错后轮询间隔倍增,上限 `MAX_POLL_INTERVAL`.
fn backoff(interval: &mut u64) {
    let new = std::cmp::min(
        MAX_POLL_INTERVAL,
        (*interval as f64 * BACKOFF_FACTOR) as u64,
    );
    if new != *interval {
        *interval = new;
    }
}

/// 解析 IMAP INTERNALDATE(`17-Jul-2024 12:34:56 +0800`)为距今秒数("现在 - 邮件时间").
fn parse_internaldate_age_secs(s: &str) -> Option<i64> {
    let ts = parse_internaldate_unix(s)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(now - ts)
}

/// INTERNALDATE 转为 UTC unix 时间戳(秒).
fn parse_internaldate_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    // 格式:dd-Mon-yyyy hh:mm:ss +zzzz
    let mut parts = s.splitn(2, ' ');
    let date = parts.next()?;
    let rest = parts.next()?;
    let (time, tz) = rest.rsplit_once(' ')?;

    let (day, mon, year) = date
        .split_once('-')
        .and_then(|(d, rest)| rest.split_once('-').map(|(m, y)| (d, m, y)))?;
    let day: u32 = day.parse().ok()?;
    let year: i32 = year.parse().ok()?;
    let month: u32 = month_num(mon)?;
    let (hh, mm, ss) = time
        .split_once(':')
        .and_then(|(h, rest)| rest.split_once(':').map(|(m, s)| (h, m, s)))?;
    let hh: u32 = hh.parse().ok()?;
    let mm: u32 = mm.parse().ok()?;
    let ss: u32 = ss.parse().ok()?;
    let tz_secs = parse_tz_offset(tz)?;

    Some(civil_to_unix(year, month, day, hh, mm, ss) - tz_secs)
}

fn month_num(name: &str) -> Option<u32> {
    match name {
        "Jan" => Some(1),
        "Feb" => Some(2),
        "Mar" => Some(3),
        "Apr" => Some(4),
        "May" => Some(5),
        "Jun" => Some(6),
        "Jul" => Some(7),
        "Aug" => Some(8),
        "Sep" => Some(9),
        "Oct" => Some(10),
        "Nov" => Some(11),
        "Dec" => Some(12),
        _ => None,
    }
}

/// 时区偏移 +0800 / -0500 → 秒(加回到 UTC 需要减掉,用正值表示东八区先加上).
fn parse_tz_offset(tz: &str) -> Option<i64> {
    let bytes = tz.as_bytes();
    if bytes.len() != 5 {
        return None;
    }
    let sign = match bytes[0] {
        b'+' => 1i64,
        b'-' => -1i64,
        _ => return None,
    };
    let hh = tz[1..3].parse::<i64>().ok()?;
    let mm = tz[3..5].parse::<i64>().ok()?;
    // +0800 表示比 UTC 早 8h;要得到 UTC 就减去该偏移,因此这里返回正值供外层减.
    Some(sign * (hh * 3600 + mm * 60))
}

fn civil_to_unix(year: i32, month: u32, day: u32, hh: u32, mm: u32, ss: u32) -> i64 {
    // 1970-01-01 以来的天数:Chronological Julian Day 偏移.
    let y = year as i64;
    let m = month as i64;
    let d = day as i64;
    let a = (14 - m) / 12;
    let yj = y + 4800 - a;
    let mj = m + 12 * a - 3;
    let jdn = d + (153 * mj + 2) / 5 + 365 * yj + yj / 4 - yj / 100 + yj / 400 - 32045;
    let unix_day = jdn - 2440588;
    unix_day * 86400 + hh as i64 * 3600 + mm as i64 * 60 + ss as i64
}
