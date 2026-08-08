//! 手写的主线程事件循环, 替代 `tao::EventLoop`.
//!
//! `tao` 是个 300+ KiB 的大 crate, 我们只需要它的两个能力: 让主线程活着 +
//! 跑平台消息泵. 直接对接 Win32 / Cocoa 可以砍掉整个 `tao` 依赖.
//!
//! - Windows: `MsgWaitForMultipleObjects` 同时监听一个唤醒事件和 QS_ALLINPUT,
//!   既能处理 tray-icon 的窗口消息, 也能让 WinRT toast 的 COM 回调通过标准
//!   消息泵派发. SMS/邮件线程投递完事件后调用 `wakeup()` 触发事件唤醒循环.
//! - macOS: NSApplication 以 `Accessory` 模式启动 (无 dock 图标), 主循环
//!   用 `nextEventMatchingMask:untilDate:` 50ms 轮询, 跨线程事件直接通过
//!   `mpsc` 通道在每次轮询边界排空, 不需要额外的 `performSelector` 机制.

#[cfg(windows)]
mod platform {
    use std::sync::mpsc::Receiver;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        QS_ALLINPUT, WM_QUIT,
    };

    /// windows-sys 0.61 没有导出 `WAIT_OBJECT_0` 常量, 但它就是
    /// `STATUS_WAIT_0 + 0 = 0`; `WAIT_OBJECT_0 + 1` 即 1. 写死比启用
    /// 整个 Threading 头方便.
    const WAIT_OBJECT_0: u32 = 0;

    /// 唤醒事件句柄, 用 usize 而不是 HANDLE (*mut c_void) 存储, 这样
    /// `OnceLock` 才能在多个线程间共享 (后者默认不是 Send/Sync).
    static WAKE_EVENT: OnceLock<usize> = OnceLock::new();

    /// 主线程初始化, 在 `main` 入口立刻调用一次.
    /// - 创建手动重置事件, 初始未触发.
    /// - 初始化 COM STA, 让 `tauri-winrt-notification` 的 on_activated
    ///   回调能通过本线程的消息队列被派发.
    pub fn install() {
        unsafe {
            let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
            let _ = WAKE_EVENT.set(event as usize);
            // windows-sys 0.61 把 COINIT_* 常量类型定成 i32, CoInitializeEx
            // 第二个参数要 u32, 这里用 as 强转 (值 = 0x2 永远是正数).
            let _ = CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED as u32);
        }
    }

    /// 任意线程均可调用, 通知主循环排空 UI 事件.
    /// 安全: SetEvent 是线程安全的内核操作, `WAKE_EVENT.get()` 读 OnceLock
    /// 内部原子变量, 也不需要 &mut.
    pub fn wakeup() {
        if let Some(&handle) = WAKE_EVENT.get() {
            unsafe { SetEvent(handle as HANDLE) };
        }
    }

    /// 阻塞主线程, 持续处理 UI 事件 + 平台消息. `!` 表示永不返回 (退出走
    /// `std::process::exit`).
    pub fn run<E, H: Fn(E)>(rx: Receiver<E>, handler: H) -> ! {
        let handle = *WAKE_EVENT
            .get()
            .expect("event_loop::install() must be called before run()") as HANDLE;
        let mut msg: MSG = unsafe { std::mem::zeroed() };

        loop {
            // 1. 排空当前所有待处理 UI 事件.
            while let Ok(event) = rx.try_recv() {
                handler(event);
            }

            // 2. 同步等待唤醒事件 / 任意窗口消息 / 100ms 超时兜底.
            //    100ms 是为了让 SMS 线程投递的 wakeup 不会因任何边角情况
            //    长期得不到响应. 参数顺序: nCount, pHandles, fWaitAll,
            //    dwMilliseconds, dwWakeMask —— QS_ALLINPUT 必须在最后
            //    (作为唤醒掩码), 100 才是超时毫秒.
            let result = unsafe {
                MsgWaitForMultipleObjects(1, &handle, 0, 100, QS_ALLINPUT)
            };

            if result == WAIT_OBJECT_0 {
                // 唤醒事件: 创建时 bManualReset=1, 唤醒后保持置位; 我们在下
                // 一轮 while let 排空 ui_rx 后会通过再次 MsgWaitFor 等待,
                // 因此无需显式 ResetEvent.
                continue;
            } else if result == WAIT_OBJECT_0 + 1 {
                // QS_ALLINPUT 触发: 一次性排空消息队列.
                loop {
                    let peek = unsafe {
                        PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE)
                    };
                    if peek == 0 {
                        break;
                    }
                    if msg.message == WM_QUIT {
                        std::process::exit(0);
                    }
                    unsafe {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            // timeout 或 error: 直接回到 while let 排空 ui_rx.
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::sync::mpsc::Receiver;
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSString;

    /// NSApplicationActivationPolicyAccessory = 1 (不进 Dock, 不抢主菜单).
    const ACCESSORY: isize = 1;
    /// NSAnyEventMask.
    const ANY_EVENT_MASK: u64 = u64::MAX;
    /// 50ms 一次轮询, 兼顾 UI 事件响应延迟与 CPU 占用.
    const POLL_INTERVAL_SECS: f64 = 0.05;

    /// 必须在主线程 (main) 调用一次, 完成 NSApplication 初始化.
    pub fn install() {
        unsafe {
            let app: *mut AnyObject =
                msg_send![objc2::class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, setActivationPolicy: ACCESSORY];
        }
    }

    /// 空操作: 50ms 轮询天然就是唤醒机制, 不需要外部触发.
    pub fn wakeup() {}

    /// 阻塞主线程, 持续处理 UI 事件 + Cocoa 事件.
    pub fn run<E, H: Fn(E)>(rx: Receiver<E>, handler: H) -> ! {
        let mode = NSString::from_str("kCFRunLoopDefaultMode");
        let app: *mut AnyObject = unsafe {
            msg_send![objc2::class!(NSApplication), sharedApplication]
        };

        loop {
            // 1. 排空当前所有待处理 UI 事件.
            while let Ok(event) = rx.try_recv() {
                handler(event);
            }

            // 2. 拉取下一个 Cocoa 事件, 50ms 超时避免空转. 拿到的 NSEvent
            //    立即送回 NSApplication 让它按类型分派 (托盘/菜单回调在
            //    这里被激活).
            unsafe {
                let date: *mut AnyObject = msg_send![
                    objc2::class!(NSDate),
                    dateWithTimeIntervalSinceNow: POLL_INTERVAL_SECS
                ];
                let event: *mut AnyObject = msg_send![
                    app,
                    nextEventMatchingMask: ANY_EVENT_MASK,
                    untilDate: date,
                    inMode: &*mode,
                    dequeue: true
                ];
                if !event.is_null() {
                    let _: () = msg_send![app, sendEvent: event];
                }
            }
        }
    }
}

pub use platform::{install, run, wakeup};
