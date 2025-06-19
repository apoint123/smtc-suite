# smtc-suite

[![Crates.io](https://img.shields.io/crates/v/smtc-suite.svg)](https://crates.io/crates/smtc-suite)
[![Docs.rs](https://docs.rs/smtc-suite/badge.svg)](https://docs.rs/smtc-suite)

一个用于与 Windows 系统媒体传输控件 (SMTC) 和系统音频进行交互的 Rust 库。

`smtc-suite` 提供了一套安全、高效的 API，用于监听和控制 Windows 上的媒体播放、捕获系统音频输出，以及管理单个应用的音量。

---

## 核心功能

* **会话监控**: 自动发现系统中所有支持 SMTC 的媒体源（如 Spotify, QQ 音乐等），并获取当前播放的曲目信息（标题、艺术家、专辑、封面）。
* **播放控制**: 对当前活动的媒体会话发送播放、暂停、切歌、跳转等控制命令。
* **音频捕获**: 捕获系统正在播放的音频流，并重采样到统一格式。
* **音量控制**: 查找特定应用的音频会话，并获取或设置其音量。
* **异步驱动**: 所有后台操作都在一个独立的、高效的异步工作线程中进行，通过通道与主应用通信，不会阻塞你的应用主线程。

## 安装

将 `smtc-suite` 添加到你的 `Cargo.toml` 文件中：

```toml
[dependencies]
smtc-suite = "*"
```

## 使用方法

与本库交互的唯一入口是 `MediaManager::start()` 函数。

1.  调用 `MediaManager::start()` 会启动所有必需的后台服务，并返回一个 `Result<MediaController>`。
2.  `MediaController` 结构体是你与后台服务交互的句柄。它包含两个字段：
    * `command_tx`: 一个 `Sender<MediaCommand>`，用于向后台发送指令。
    * `update_rx`: 一个 `Receiver<MediaUpdate>`，用于接收来自后台的状态更新和事件。
3.  你可以在一个独立的线程中循环监听 `update_rx` 以接收实时更新。
4.  通过 `command_tx` 发送 `MediaCommand` 枚举中的指令来控制后台服务。
5.  当你的应用退出时，务必调用 `controller.shutdown()` 或发送一个 `MediaCommand::Shutdown` 来优雅地关闭后台线程。

## 示例

下面是一个简单的示例，演示如何启动服务、监听事件并最终关闭它。

```rust
use smtc_suite::{MediaManager, MediaCommand, MediaUpdate, SmtcError};
use std::thread;
use std::time::Duration;

fn main() -> Result<(), SmtcError> {
    // 1. 启动媒体服务并获取控制器
    let controller = MediaManager::start()?;
    println!("SMTC Suite 服务已启动。");

    // 2. 在一个单独的线程中处理来自后台的更新事件
    let update_receiver = controller.update_rx;
    let update_thread = thread::spawn(move || {
        // 循环接收更新，直到通道关闭
        while let Ok(update) = update_receiver.recv() {
            match update {
                MediaUpdate::TrackChanged(info) => {
                    println!(
                        "曲目变更: {} - {}",
                        info.artist.as_deref().unwrap_or("未知艺术家"),
                        info.title.as_deref().unwrap_or("未知标题")
                    );
                }
                MediaUpdate::SessionsChanged(sessions) => {
                    println!("可用媒体会话列表已更新，共 {} 个。", sessions.len());
                    // 打印所有会话的显示名称
                    for session in sessions {
                        println!("  - {}", session.display_name);
                    }
                }
                _ => { /* 可以处理其他更新，如 音频数据、音量变化等 */ }
            }
        }
        println!("更新通道已关闭，事件监听线程退出。");
    });

    // 3. 在主线程中，你可以发送命令
    // 例如，等待5秒后尝试播放（如果当前是暂停状态）
    println!("将在5秒后发送“播放”命令...");
    thread::sleep(Duration::from_secs(5));
    controller.command_tx.send(MediaCommand::Control(smtc_suite::SmtcControlCommand::Play))?;

    // 再等待10秒
    println!("将在10秒后关闭服务...");
    thread::sleep(Duration::from_secs(10));

    // 4. 在程序退出前，发送关闭命令
    println!("正在发送关闭命令...");
    controller.shutdown()?;

    // 5. 等待更新线程结束，确保所有消息都被处理
    update_thread.join().expect("更新线程 join 失败");

    println!("程序已优雅退出。");

    Ok(())
}
```

## 许可证

本项目采用 **MIT** 许可证。你可以随意使用本项目的任何代码。