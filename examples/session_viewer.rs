use smtc_suite::{MediaManager, MediaUpdate, NowPlayingInfo};
use std::time::Duration;

/// 解析 Apple Music 发送的 "艺术家 — 专辑" 复合字段。
fn parse_combined_artist_album_info(mut info: NowPlayingInfo) -> NowPlayingInfo {
    if let Some(original_artist_field) = info.artist.take() {
        if let Some((artist, album)) = original_artist_field.split_once(" — ") {
            info.artist = Some(artist.trim().to_string());
            if info.album_title.as_deref().unwrap_or("").is_empty() {
                info.album_title = Some(album.trim().to_string());
            }
        } else {
            info.artist = Some(original_artist_field);
        }
    }
    info
}

/// 重新计算并获取最新的估算播放位置（毫秒）。
fn get_estimated_pos(info: &NowPlayingInfo) -> Option<u64> {
    if info.is_playing.unwrap_or(false)
        && let (Some(last_pos_ms), Some(report_time)) =
            (info.position_ms, info.position_report_time)
    {
        let elapsed_ms = report_time.elapsed().as_millis() as u64;
        let estimated_pos = last_pos_ms + elapsed_ms;
        if let Some(duration_ms) = info.duration_ms
            && duration_ms > 0
        {
            return Some(estimated_pos.min(duration_ms));
        }
        return Some(estimated_pos);
    }
    info.position_ms
}

/// 将毫秒格式化为 MM:SS.ms
fn ms_to_hms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let mins = secs / 60;
    let secs = secs % 60;
    format!("{:02}:{:02}.{:03}", mins, secs, millis)
}

/// 一个专门负责打印SMTC状态日志的函数
fn log_smtc_status(info: &NowPlayingInfo) {
    let estimated_pos_ms = get_estimated_pos(info).unwrap_or(0);
    let duration_ms = info.duration_ms.unwrap_or(0);

    let elapsed_since_report = info
        .position_report_time
        .map(|t| t.elapsed().as_millis())
        .unwrap_or(0);

    log::info!(
        "SMTC 状态更新: {}/{} [估算位置={}ms (原始: {}ms + 经过: {}ms), 播放中={}, 总时长={}ms]",
        ms_to_hms(estimated_pos_ms),
        ms_to_hms(duration_ms),
        estimated_pos_ms,
        info.position_ms.unwrap_or(0),
        elapsed_since_report,
        info.is_playing.unwrap_or(false),
        duration_ms
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (controller, mut update_rx) = MediaManager::start()?;

    let mut last_known_info: Option<NowPlayingInfo> = None;
    let mut interval = tokio::time::interval(Duration::from_millis(500));

    loop {
        tokio::select! {
            biased; // 优先处理来自通道的消息

            // 分支 1: 等待来自后台的更新
            maybe_update = update_rx.recv() => {
                let update = match maybe_update {
                    Some(u) => u,
                    None => {
                        // recv() 返回 None 意味着通道已关闭
                        log::info!("媒体事件通道已关闭，程序退出。");
                        break;
                    }
                };

                match update {
                    MediaUpdate::SessionsChanged(sessions) => {
                        if sessions.is_empty() {
                            log::warn!("当前没有可用的媒体会话。");
                        } else {
                            for (i, session) in sessions.iter().enumerate() {
                                log::info!("  [{}] {}", i + 1, session.display_name);
                            }
                        }
                    }
                    MediaUpdate::TrackChanged(info) | MediaUpdate::TrackChangedForced(info) => {
                        let new_info = parse_combined_artist_album_info(info);

                        let has_text_info_changed = match &last_known_info {
                            Some(last) => {
                                last.title != new_info.title
                                    || last.artist != new_info.artist
                                    || last.album_title != new_info.album_title
                            }
                            None => true,
                        };

                        if has_text_info_changed {
                            let title = new_info.title.as_deref().unwrap_or("N/A");
                            let artist = new_info.artist.as_deref().unwrap_or("N/A");
                            let album = new_info.album_title.as_deref().unwrap_or("N/A");
                            log::info!("曲目信息变更: {} - {} (专辑: {})", artist, title, album);
                        }

                        log_smtc_status(&new_info);
                        last_known_info = Some(new_info);
                    }
                    MediaUpdate::VolumeChanged {
                        session_id,
                        volume,
                        is_muted,
                    } => {
                        let vol_percent = (volume * 100.0).round() as u8;
                        let mute_str = if is_muted { " (已静音)" } else { "" };
                        log::info!(
                            "音量变更 -> 会话 '{}': {}%{}",
                            session_id,
                            vol_percent,
                            mute_str
                        );
                    }
                    MediaUpdate::SelectedSessionVanished(session_id) => {
                        log::warn!("当前选择的会话 '{}' 已消失。", session_id);
                        last_known_info = None;
                    }
                    MediaUpdate::Error(e) => {
                        log::error!("运行时错误: {}", e);
                    }
                    MediaUpdate::AudioData(_) => {}
                    MediaUpdate::Diagnostic(_) => {}
                }
            },

            // 分支 2: 每 500ms 触发一次，用于定时刷新状态日志
            _ = interval.tick() => {
                if let Some(info) = &last_known_info
                    && info.is_playing.unwrap_or(false) {
                        log_smtc_status(info);
                    }
            }
        }
    }

    controller.shutdown().await?;

    Ok(())
}
