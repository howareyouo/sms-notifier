//! System tray icon and native context menu.
//!
//! 通过独立的 `tray-icon` crate 实现: 回调直接把事件投递到一个共享
//! `mpsc::SyncSender<AppEvent>`, 由 `event_loop::run` 在每次循环顶部排空
//! 并唤醒主循环. 完全不再依赖 `tao` (一个 ~300 KiB 的大窗口管理 crate),
//! 显著减小二进制体积.
use std::sync::mpsc::SyncSender;

use tray_icon::menu::{Menu, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::AppEvent;

// 32x32 RGBA payload 已在 build.rs 解码后写入 OUT_DIR, 这里直接嵌入.
// 同时 include 维度常量以便在编译期校验 (尺寸 > 64 会在 build.rs 报错).
include!(concat!(env!("OUT_DIR"), "/tray_icon.rs"));

/// 菜单项 ID. 与 build 时的 MenuItem::with_id 一一对应.
pub const LOG_MENU_ID: &str = "log";
pub const EXIT_MENU_ID: &str = "exit";

/// 构建托盘图标 + 三项右键菜单 (About / Log / Exit), 并把原生事件转发到
/// 给定的 `AppEvent` 通道.
///
/// 调用方必须持有返回的 `TrayIcon` 直到进程退出, 否则托盘会随 Drop 一起
/// 消失. 全局 `set_event_handler` 是覆盖式的, 多次调用本函数会覆盖回调.
pub fn build(app_event_tx: SyncSender<AppEvent>) -> Result<TrayIcon, String> {
    let icon = Icon::from_rgba(
        include_bytes!(concat!(env!("OUT_DIR"), "/tray_icon.rgba")).to_vec(),
        TRAY_ICON_WIDTH,
        TRAY_ICON_HEIGHT,
    )
    .map_err(|e| format!("failed to create tray icon: {e}"))?;

    // 三项菜单: 不可点击的版本号标签, 打开 /logs, 退出. ID 字符串以
    // 顶层常量 LOG_MENU_ID / EXIT_MENU_ID 为准, main 那边据此匹配事件.
    let menu = Menu::new();
    menu.append(&MenuItem::with_id(
        MenuId::new("about"),
        "SMS Notifier v0.1.0",
        false, // disabled: 仅作为版本号展示
        None,
    ))
    .map_err(|e| format!("menu append (about): {e}"))?;
    menu.append(&MenuItem::with_id(
        MenuId::new(LOG_MENU_ID),
        "Log",
        true,
        None,
    ))
    .map_err(|e| format!("menu append (log): {e}"))?;
    menu.append(&MenuItem::with_id(
        MenuId::new(EXIT_MENU_ID),
        "Exit",
        true,
        None,
    ))
    .map_err(|e| format!("menu append (exit): {e}"))?;

    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("SMS Notifier")
        .build()
        .map_err(|e| format!("failed to build system tray: {e}"))?;

    // tray-icon 的事件回调要求 Fn + Send + Sync + 'static, SyncSender 既
    // Clone 又 Send + Sync, 多个回调各自持有一份 clone 即可.
    let tx = app_event_tx.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tx.send(AppEvent::TrayIconEvent(event));
    }));

    let tx = app_event_tx;
    tray_icon::menu::MenuEvent::set_event_handler(Some(move |event| {
        let _ = tx.send(AppEvent::MenuEvent(event));
    }));

    Ok(tray)
}
