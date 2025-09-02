use smtc_suite::{Controls, MediaManager, MediaUpdate, NowPlayingInfo, PlaybackStatus, RepeatMode};
use std::time::Duration;

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

fn get_estimated_pos(info: &NowPlayingInfo) -> Option<u64> {
    if info.playback_status == Some(PlaybackStatus::Playing)
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

fn ms_to_hms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let mins = secs / 60;
    let secs = secs % 60;
    format!("{mins:02}:{secs:02}.{millis:03}")
}

fn controls_status(info: &NowPlayingInfo) {
    let to_symbol = |flag: bool| if flag { "True" } else { "False" };
    let controls = info.controls.unwrap_or_default();

    log::info!(
        "可用操作: [播放: {}], [暂停: {}], [上一首: {}], [下一首: {}], [跳转: {}], [随机: {}], [循环: {}]",
        to_symbol(controls.contains(Controls::CAN_PLAY)),
        to_symbol(controls.contains(Controls::CAN_PAUSE)),
        to_symbol(controls.contains(Controls::CAN_SKIP_PREVIOUS)),
        to_symbol(controls.contains(Controls::CAN_SKIP_NEXT)),
        to_symbol(controls.contains(Controls::CAN_SEEK)),
        to_symbol(controls.contains(Controls::CAN_CHANGE_SHUFFLE)),
        to_symbol(controls.contains(Controls::CAN_CHANGE_REPEAT))
    );
}

fn timeline_status(info: &NowPlayingInfo) {
    let estimated_pos_ms = get_estimated_pos(info).unwrap_or(0);
    let duration_ms = info.duration_ms.unwrap_or(0);

    log::info!(
        "时间线: {} / {}",
        ms_to_hms(estimated_pos_ms),
        ms_to_hms(duration_ms),
    );
}

fn log_playback_status(info: &NowPlayingInfo) {
    let play_state = match info.playback_status {
        Some(PlaybackStatus::Playing) => "正在播放",
        Some(PlaybackStatus::Paused) => "已暂停",
        _ => "已停止",
    };
    let shuffle_state = if info.is_shuffle_active.unwrap_or(false) {
        "True"
    } else {
        "False"
    };
    let repeat_state = match info.repeat_mode {
        Some(RepeatMode::Off) => "关闭",
        Some(RepeatMode::One) => "单曲",
        Some(RepeatMode::All) => "列表",
        None => "未知",
    };

    log::info!("播放状态: {play_state} | 随机: {shuffle_state} | 循环: {repeat_state}");
}

fn handle_track_changed(info: NowPlayingInfo, last_known_info: &mut Option<NowPlayingInfo>) {
    let new_info = parse_combined_artist_album_info(info);

    let has_text_info_changed = match last_known_info {
        Some(last) => {
            last.title != new_info.title
                || last.artist != new_info.artist
                || last.album_title != new_info.album_title
        }
        None => true,
    };

    let has_controls_changed = match last_known_info {
        Some(last) => last.controls != new_info.controls,
        None => true,
    };

    let has_playback_state_changed = match last_known_info {
        Some(last) => {
            last.playback_status != new_info.playback_status
                || last.is_shuffle_active != new_info.is_shuffle_active
                || last.repeat_mode != new_info.repeat_mode
        }
        None => true,
    };

    if has_text_info_changed {
        let title = new_info.title.as_deref().unwrap_or("N/A");
        let artist = new_info.artist.as_deref().unwrap_or("N/A");
        let album = new_info.album_title.as_deref().unwrap_or("N/A");
        log::info!("曲目信息变更: {artist} - {title} (专辑: {album})");
    }

    if has_controls_changed {
        controls_status(&new_info);
    }

    if has_playback_state_changed {
        log_playback_status(&new_info);
    }

    timeline_status(&new_info);
    *last_known_info = Some(new_info);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("trace")).init();

    let (controller, mut update_rx) = MediaManager::start()?;
    let mut last_known_info: Option<NowPlayingInfo> = None;
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            biased; // 优先处理来自通道的消息

            // 分支 1: 等待来自后台的更新
            maybe_update = update_rx.recv() => {
                let Some(update) = maybe_update else {
                     log::info!("媒体事件通道已关闭，程序退出。");
                     break;
                 };

                match update {
                    MediaUpdate::SessionsChanged(sessions) => {
                        if sessions.is_empty() {
                            log::warn!("当前没有可用的媒体会话。");
                            last_known_info = None;
                        } else {
                            for (i, session) in sessions.iter().enumerate() {
                                log::info!("  [{}] {}", i + 1, session.display_name);
                            }
                        }
                        println!();
                    }
                    MediaUpdate::TrackChanged(info) => {
                        handle_track_changed(info, &mut last_known_info);
                    }
                    MediaUpdate::SelectedSessionVanished(session_id) => {
                        log::warn!("当前选择的会话 '{session_id}' 已消失。");
                        last_known_info = None;
                    }
                    MediaUpdate::Error(e) => {
                        log::error!("运行时错误: {e}");
                    }
                    _ => {}
                }
            },

            // 分支 2: 定时器触发，用于更新时间线
            _ = interval.tick() => {
                if let Some(info) = &last_known_info
                    && info.playback_status == Some(PlaybackStatus::Playing)
                {
                        timeline_status(info);
                    }
            }
        }
    }

    controller.shutdown().await?;

    Ok(())
}
