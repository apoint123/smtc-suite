use log::{error, info};
use smtc_suite::{MediaCommand, MediaManager, MediaUpdate, NowPlayingInfo, PlaybackStatus};
use tokio::signal;

fn ms_to_hms(ms: u64) -> String {
    let secs = ms / 1000;
    let ms = ms % 1000;
    let mins = secs / 60;
    let secs = secs % 60;
    let mins = mins % 60;
    format!("{mins:02}:{secs:02}.{ms:03}")
}

fn print_track_update(info: &NowPlayingInfo) {
    let estimated_pos = info.position_ms.unwrap_or(0);
    let original_pos = info.smtc_position_ms.unwrap_or(0);
    let duration = info.duration_ms.unwrap_or(0);

    log::info!(
        "SMTC 状态更新: {}/{} [原始位置={}, 播放中={}, 总时长={}ms]",
        ms_to_hms(estimated_pos),
        ms_to_hms(duration),
        ms_to_hms(original_pos),
        info.playback_status == Some(PlaybackStatus::Playing),
        duration
    );
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (controller, mut update_rx) = match MediaManager::start() {
        Ok(res) => res,
        Err(e) => {
            error!("无法启动媒体服务: {e:?}");
            return;
        }
    };

    if let Err(e) = controller
        .command_tx
        .send(MediaCommand::SetHighFrequencyProgressUpdates(true))
        .await
    {
        error!("无法启用高频进度更新: {e:?}");
    }

    let main_loop = async {
        let mut last_print_time = std::time::Instant::now();
        let mut last_track_id: Option<(String, String)> = None;

        loop {
            if let Some(update) = update_rx.recv().await {
                match update {
                    MediaUpdate::TrackChanged(info) => {
                        let current_track_id = info.title.clone().zip(info.artist.clone());

                        let is_new_track = last_track_id != current_track_id;
                        let a_second_has_passed =
                            last_print_time.elapsed() >= std::time::Duration::from_secs(1);

                        if is_new_track || a_second_has_passed {
                            print_track_update(&info);
                            last_print_time = std::time::Instant::now();
                            last_track_id = current_track_id;
                        }
                    }
                    other_update => {
                        print_other_updates(other_update);
                    }
                }
            } else {
                error!("更新通道已关闭。程序退出。");
                break;
            }
        }
    };

    let ctrl_c_handler = async {
        signal::ctrl_c().await.expect("无法监听 Ctrl+C 信号");
        if let Err(e) = controller.shutdown().await {
            error!("关闭时出错: {e:?}");
        }
    };

    tokio::select! {
        () = main_loop => {},
        () = ctrl_c_handler => {},
    }
}
fn print_other_updates(update: MediaUpdate) {
    match update {
        MediaUpdate::SessionsChanged(sessions) => {
            if sessions.is_empty() {
                info!("[SessionsChanged] 没有可用的媒体会话。");
            } else {
                info!("[SessionsChanged] 发现 {} 个会话:", sessions.len());
                for (i, session) in sessions.iter().enumerate() {
                    info!(
                        "    {}. \"{}\" (ID: {})",
                        i + 1,
                        session.display_name,
                        session.session_id
                    );
                }
            }
        }
        MediaUpdate::VolumeChanged {
            session_id,
            volume,
            is_muted,
        } => {
            info!(
                "[VolumeChanged] 音量改变, 会话 ID: {session_id}, 音量: {volume}, 静音: {is_muted}"
            );
        }
        MediaUpdate::SelectedSessionVanished(session_id) => {
            info!("[SelectedSessionVanished] 选择的会话已消失: {session_id}");
        }
        MediaUpdate::Error(err_msg) => {
            error!("[Error] 错误: {err_msg}");
        }
        MediaUpdate::Diagnostic(diag) => {
            info!("[Diagnostic] 诊断信息: {}", diag.message);
        }
        MediaUpdate::TrackChanged(_) | MediaUpdate::AudioData(_) => {}
    }
}
