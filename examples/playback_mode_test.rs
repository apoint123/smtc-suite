use log::{debug, info, warn};
use smtc_suite::{MediaCommand, MediaManager, MediaUpdate, RepeatMode, SmtcControlCommand};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("正在启动媒体服务...");
    let controller = MediaManager::start()?;
    let command_tx = controller.command_tx;

    let command_tx_clone = command_tx.clone();

    let session_selected = Arc::new(AtomicBool::new(false));
    let session_selected_clone = session_selected.clone();

    let update_thread = thread::spawn(move || {
        while let Ok(update) = controller.update_rx.recv() {
            match update {
                MediaUpdate::TrackChanged(info) => {
                    info!(
                        "当前会话随机播放: {:?}",
                        info.is_shuffle_active.unwrap_or(false)
                    );
                    info!(
                        "当前会话重复播放: {:?}",
                        info.repeat_mode.unwrap_or_default()
                    );
                }
                MediaUpdate::SessionsChanged(sessions) => {
                    debug!("可用会话列表已更新，共 {} 个。", sessions.len());
                    if let Some(first_session) = sessions.first() {
                        if !session_selected_clone.load(Ordering::Relaxed) {
                            info!(
                                "发现媒体会话 '{}'，将自动选择。",
                                first_session.display_name
                            );
                            if command_tx_clone
                                .send(MediaCommand::SelectSession(
                                    first_session.session_id.clone(),
                                ))
                                .is_ok()
                            {
                                session_selected_clone.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
                MediaUpdate::Error(e) => {
                    warn!("收到一个非致命错误: {}", e);
                }
                _ => { /* 忽略其他更新 */ }
            }
        }
        info!("更新通道已关闭，事件监听线程退出。");
    });

    for _ in 0..10 {
        if session_selected.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }

    if !session_selected.load(Ordering::Relaxed) {
        warn!("未能自动选择会话。测试将继续，但控制命令可能无效。");
    }

    thread::sleep(Duration::from_secs(3));
    info!("开启随机播放");
    command_tx.send(MediaCommand::Control(SmtcControlCommand::SetShuffle(true)))?;

    thread::sleep(Duration::from_secs(3));
    info!("设置重复模式为“全部循环”");
    command_tx.send(MediaCommand::Control(SmtcControlCommand::SetRepeatMode(
        RepeatMode::All,
    )))?;

    thread::sleep(Duration::from_secs(5));
    info!("关闭随机播放");
    command_tx.send(MediaCommand::Control(SmtcControlCommand::SetShuffle(false)))?;

    thread::sleep(Duration::from_secs(3));
    info!("关闭重复模式");
    command_tx.send(MediaCommand::Control(SmtcControlCommand::SetRepeatMode(
        RepeatMode::Off,
    )))?;

    thread::sleep(Duration::from_secs(2));

    info!("测试完成，正在发送关闭命令...");
    command_tx.send(MediaCommand::Shutdown)?;

    update_thread.join().expect("更新线程 join 失败");

    Ok(())
}
