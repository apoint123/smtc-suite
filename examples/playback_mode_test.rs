use log::{debug, info, warn};
use smtc_suite::{MediaCommand, MediaManager, MediaUpdate, RepeatMode, SmtcControlCommand};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("正在启动媒体服务...");
    let (controller, mut update_rx) = MediaManager::start()?;
    let command_tx = controller.command_tx.clone();

    let session_selected = Arc::new(AtomicBool::new(false));
    let session_selected_clone = session_selected.clone();
    let command_tx_clone = command_tx.clone();

    let event_task = tokio::spawn(async move {
        while let Some(update) = update_rx.recv().await {
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
                    if let Some(first_session) = sessions.first()
                        && !session_selected_clone.load(Ordering::Relaxed)
                    {
                        info!(
                            "发现媒体会话 '{}'，将自动选择。",
                            first_session.display_name
                        );
                        if command_tx_clone
                            .send(MediaCommand::SelectSession(
                                first_session.session_id.clone(),
                            ))
                            .await
                            .is_ok()
                        {
                            session_selected_clone.store(true, Ordering::Relaxed);
                        }
                    }
                }
                MediaUpdate::Error(e) => {
                    warn!("收到一个非致命错误: {}", e);
                }
                _ => { /* 忽略其他更新 */ }
            }
        }
        info!("更新通道已关闭，事件监听任务退出。");
    });

    for _ in 0..10 {
        if session_selected.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    if !session_selected.load(Ordering::Relaxed) {
        warn!("未能自动选择会话。测试将继续，但控制命令可能无效。");
    }

    tokio::time::sleep(Duration::from_secs(3)).await;
    info!("开启随机播放");
    command_tx
        .send(MediaCommand::Control(SmtcControlCommand::SetShuffle(true)))
        .await?;

    tokio::time::sleep(Duration::from_secs(3)).await;
    info!("设置重复模式为“全部循环”");
    command_tx
        .send(MediaCommand::Control(SmtcControlCommand::SetRepeatMode(
            RepeatMode::All,
        )))
        .await?;

    tokio::time::sleep(Duration::from_secs(5)).await;
    info!("关闭随机播放");
    command_tx
        .send(MediaCommand::Control(SmtcControlCommand::SetShuffle(false)))
        .await?;

    tokio::time::sleep(Duration::from_secs(3)).await;
    info!("关闭重复模式");
    command_tx
        .send(MediaCommand::Control(SmtcControlCommand::SetRepeatMode(
            RepeatMode::Off,
        )))
        .await?;

    tokio::time::sleep(Duration::from_secs(2)).await;

    info!("测试完成，正在发送关闭命令...");
    controller.shutdown().await?;

    event_task.await?;

    Ok(())
}
