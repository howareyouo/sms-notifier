# SMS Notifier

[![Build](https://github.com/howareyouo/sms-notifier/actions/workflows/build.yml/badge.svg)](https://github.com/howareyouo/sms-notifier/actions/workflows/build.yml)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-windows%20%7C%20macOS-blue.svg)](https://github.com/howareyouo/sms-notifier)
[![Version](https://img.shields.io/github/v/release/howareyouo/sms-notifier)](https://github.com/howareyouo/sms-notifier/releases)
[![License](https://img.shields.io/badge/license-MIT-yellow.svg)](LICENSE)

**简体中文** | [English](README.en.md)

> 一个轻量级 Windows / macOS 系统托盘工具, 通过本地 HTTP 接口接收短信 / 验证码, 或监听邮箱 IMAP 自动提取验证码, 发出通知并 **一键粘贴到当前焦点窗口并回车提交**。

专为「在 PC 上接收手机短信 / 邮件验证码」场景设计 —— 配合 IOS 的 **快捷方式** (`Shortcuts`)、Android 的 **短信转发** (如 [SmsForwarder](https://github.com/pppscn/SmsForwarder)) 或任意支持 IMAP 的邮箱, 即可实现: **收到验证码 → 自动推送到 PC → 自动复制粘贴并提交**, 全程丝滑无比! 再也不用分心去找验证码了!!!

***

## 目录

- [功能特性](#功能特性)
- [快速开始](#快速开始)
- [使用方法](#使用方法)
- [命令行参数](#命令行参数)
- [系统托盘](#系统托盘)
- [项目结构](#项目结构)
- [常见问题](#常见问题)
- [License](#license)

***

## 功能特性

- **原生通知** —— 收到验证码后调用通知中心, 点击通知可复制内容到剪贴板。

- **自动提取验证码** —— 智能解析 `【公司】您的验证码是 123456` 格式短信, 自动识别 4–8 位数字验证码。

- **自动粘贴并提交** —— 收到验证码后自动复制粘贴到当前焦点窗口(Windows `Ctrl+V` / macOS `Cmd+V`), 并模拟执行一次 `Enter` 提交。

- **邮件验证码监听** —— 可选启用: 通过 IMAP 监听邮箱, 命中关键词的邮件自动提取验证码并->复制粘贴; 支持 IDLE 实时推送与定时轮询, 需配置 `config.yaml`。

- **实时日志** —— 内置 WebSocket 实时日志, 浏览器打开 `http://127.0.0.1:<port>/logs` 即可查看带时间戳的运行日志, 仅限本机访问。

- **跨平台** —— 同时支持 Windows 10+ 和 macOS, 享同一套 HTTP 服务、短信解析、验证码提取核心逻辑。

- **小体积** —— 最终二进制约 **0.91 MiB (Windows) / 0.81 MiB (macOS)**, 远低于 1MB。


## 快速开始

### 1. 下载可执行文件

前往 [Releases](https://github.com/howareyouo/sms-notifier/releases) 页面下载对应平台的二进制文件:

- **Windows**: `sms-notifier.exe` —— 放置到任意目录后双击运行
- **macOS**: `sms-notifier` —— 放置到任意目录后通过终端运行 `chmod +x sms-notifier && ./sms-notifier`

**Windows 首次运行会:**

- 在 `%APPDATA%\Microsoft\Windows\Start Menu\Programs\` 创建 `SMS Notifier.lnk` 快捷方式(用于 Toast 通知身份)
- 在系统托盘显示图标

**macOS 首次运行需配置权限:**

| 权限                   | 用途                         | 配置路径                        |
| -------------------- | -------------------------- | --------------------------- |
| 辅助功能 (Accessibility) | 模拟 `Cmd+V` 粘贴和 `Return` 按键 | 系统设置 → 隐私与安全 → 辅助功能         |
| 通知                   | 显示原生通知                     | 系统设置 → 通知 → Terminal(或运行终端) |

> macOS 首次运行时会弹出辅助功能授权提示。若无弹窗, 动在「系统设置 → 隐私与安全 → 辅助功能」中添加运行终端(Terminal / iTerm 等)并打开开关。未授权时通知仍可显示, 粘贴和回车不生效。

### 2. 配置短信转发

在手机端 SmsForwarder(或其他支持 HTTP 转发的工具)中配置:

| 配置项     | 值                                       |
| ------- | --------------------------------------- |
| 转发方式    | `Webhook` / `HTTP GET` / `HTTP POST`    |
| 目标地址    | `http://<PC_IP>:18080/?sms={{msg}}`   |
| 或纯验证码模式 | `http://<PC_IP>:18080/?code={{code}}` |

> 同一局域网下,  `PC_IP` 换成 PC 的局域网 IP 即可。

### 3. iOS 快捷指令 (替代方案)

iPhone 用户无需安装第三方 App, 用系统自带的「快捷指令」(Shortcuts)实现相同效果:

1. 打开「快捷指令」App → 新建快捷指令
2. 添加触发条件:**「信息」** → 收件人 / 发件人 / 内容过滤(例如「信息内容」包含「验证码」「code」「verification」)
3. 添加操作 **「获取 URL 内容」**:
   - URL:`http://<PC_IP>:18080/?sms=<收到的信息>`
   - 方法:`GET`
4. 在「快捷指令」设置中开启 **「在后台运行」**, 保锁屏时也能触发

> 优点: 零依赖、系统级触发、隐私友好(数据不经过第三方 App)。
> 限制: iOS 对自动化触发有冷却时间; Message 触发在某些 iOS 版本上需要后台刷新,可安装 SmsForwarder 等专业转发 App。

### 4. 测试

在 PC 浏览器中访问以下地址, 到 Toast 通知并自动粘贴到焦点窗口即表示成功:

```
http://127.0.0.1:18080/?code=1234
http://127.0.0.1:18080/?sms=【GitHub】您的验证码是 8888, 5分钟内有效。
```

***

## 程序流程

### 传验证码:

```
GET http://127.0.0.1:18080/?code=1234
```
系统将:
1. 系统将弹出「收到验证码」通知
2. 将 `1234` 复制到剪贴板
3. 在当前焦点窗口执行粘贴与回车

### 传整条短信:

```
GET http://127.0.0.1:18080/?sms=【GitHub】您的验证码是 8888, 5分钟内有效。
```
系统将:
1. 提取 `【公司名】` 作为通知标题(`GitHub`)
2. 提取短信中 4–8 位连续数字作为验证码(`8888`)
3. 在当前焦点窗口执行粘并回车

### 通知交互

| 操作              | 行为                       |
| ---------------   | ------------------------ |
| **点击 Toast 通知** | 将验证码再次复制到剪贴板(最多重试 3 次)
| **左键点击托盘图标** | 打开日志页面
| **右键点击托盘图标** | 弹出菜单(About / Log / Exit) |

***

## 命令行参数

```
sms-notifier [--port <PORT>] [--config <PATH>]
```

| 参数               | 简写          | 默认值        | 说明                         |
| ---------------- | ----------- | ---------- | -------------------------- |
| `--port <PORT>`  | `-p <PORT>` | `18080`    | HTTP 服务监听端口                 |
| `--config <PATH>` | `-c <PATH>` | `config.yaml`(同目录) | 邮件监听配置文件路径(不指定或程序文件夹下不存在则不启用邮件监听) |
| `--help`         | `-h`        | —          | 打印帮助信息并退出                  |

> 也支持 `--port=<PORT>` / `--config=<PATH>` 等号形式。

**示例**

```powershell
# 监听 9090 端口
sms-notifier.exe --port 9090

# 指定邮件配置文件(默认读取 exe 同目录的 config.yaml)
sms-notifier.exe --config D:\config.yaml

# 查看帮助
sms-notifier.exe -h
```

***

## 系统托盘

| 菜单项                     | 行为                |
| ----------------------- | ----------------- |
| **SMS Notifier v0.1.0** | 版本信息(只读)           |
| **Log**                 | 在默认浏览器中打开日志页面      |
| **Exit**                | 退出程序              |



## 常见问题

### Q: 为什么必须创建开始菜单快捷方式?

Windows Toast 通知要求每个应用拥有稳定的 `AppUserModelID`, 该 ID 必须关联一个开始菜单快捷方式才能正常显示带图标的通知。本程序作为便携 EXE 运行, *不复制自身到任何目录**, 在 `%APPDATA%\Microsoft\Windows\Start Menu\Programs\` 创建指向当前 EXE 路径的 `.lnk`, 在快捷方式属性中写入 `AppUserModelID`。

### Q: EXE 移动位置后通知还会工作吗?

会。程序每次启动都会基于当前 EXE 路径重新创建开始菜单快捷方式, 移动位置后下次启动会自动刷新快捷方式, 通知依然正常。

### Q: 通知不弹出怎么办?

1. 确认 Windows 通知中心未关闭「专注助手」
2. 检查 `设置 → 系统 → 通知` 是否启用
3. 查看日志窗口(托盘右键 → Log)中的错误信息
4. 确认开始菜单中存在 `SMS Notifier` 快捷方式

### Q: 端口被占用怎么办?

使用 `--port` 指定其他端口:

```powershell
sms-notifier.exe --port 9090
```

### Q: 转发短信不成功(超时)怎么办?

Windows 系统防火墙阻止了入站请求。以管理员权限在CMD中运行以下命令放行：

```cmd
netsh advfirewall firewall add rule name="sms-notifier" dir=in action=allow
```

将 `sms-notifier` 替换为实际程序名 (如果你改过的话)。

macOS 前往「系统设置 → 网络 → 防火墙 → 防火墙选项」, 将运行终端 `Terminal`/`iTerm` 等添加到列表, 并确保其「**允许接入连接**」为开启状态。



### Q: 如何启用邮件验证码监听?

把仓库里的 `config.example.yaml` 复制为 `config.yaml`(放到 exe 同目录, 或用 `--config` 指定路径), 填入邮箱 IMAP 服务器、账号与授权码, 配置 `match_keywords` 关键词即可。文件不存在且不指定 `--config` 时, 邮件监听完全不启动、无额外开销。详见配置文件内的注释。

### Q: 打开 `/logs` 提示 403 Forbidden?

日志接口仅限本机访问, 请通过 `http://127.0.0.1:<port>/logs`(或 `localhost`)打开, 不要用局域网 IP 访问。

### Q: 为什么 `?code=` 空值会返回 400?

为避免焦点窗口收到空字符串粘贴后误按 `Enter` 提交空表单(可能触发误操作), 值会在服务端直接拒绝。

### Q: 支持哪些平台?

- **Windows 10+**:功能完整(Toast 通知、控制台隐藏/恢复、托盘、剪贴板、自动粘贴)
- **macOS**:核心功能完整(原生通知、托盘、剪贴板、自动粘贴), 日志经浏览器查看(托盘「Log」打开 `http://127.0.0.1:<port>/logs`)
- 其他平台:仅编译保证, 行时不支持

### Q: macOS 上粘贴/回车不生效怎么办?

macOS 要求辅助功能权限才能模拟键盘事件。前往「系统设置 → 隐私与安全 → 辅助功能」, 加运行终端(Terminal / iTerm 等)并打开开关, 后重启程序。


## License

本项目采用 [MIT License](LICENSE)。
