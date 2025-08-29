use std::io::{BufRead, stdin};

use log::{error, info};
use smtc_suite::{MediaManager, MediaUpdate};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (controller, mut update_rx) = match MediaManager::start() {
        Ok((c, rx)) => (c, rx),
        Err(e) => {
            error!("SMTC 服务启动失败: {e}");
            return Err(e.into());
        }
    };

    let update_task = tokio::spawn(async move {
        while let Some(update) = update_rx.recv().await {
            match update {
                MediaUpdate::SessionsChanged(sessions) => {
                    info!("[会话列表更新] 当前共有 {} 个媒体会话:", sessions.len());
                    for session in sessions {
                        info!(
                            "  - Session ID: {}, Display: {}",
                            session.session_id, session.display_name
                        );
                    }
                }
                MediaUpdate::Error(e) => {
                    error!("[后台错误] {e}");
                }
                _ => {}
            }
        }
    });

    info!("按 Enter 键即可退出程序。");

    let input_task = tokio::task::spawn_blocking(|| {
        let mut buffer = String::new();
        stdin().lock().read_line(&mut buffer)
    });

    input_task.await??;

    controller.shutdown().await?;

    update_task.await?;

    Ok(())
}
