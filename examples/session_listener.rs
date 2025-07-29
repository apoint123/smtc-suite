use std::io::{BufRead, stdin};
use std::thread;

use log::{error, info};
use smtc_suite::{MediaManager, MediaUpdate};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (command_tx, update_receiver) = match MediaManager::start() {
        Ok(c) => (c.command_tx, c.update_rx),
        Err(e) => {
            error!("SMTC 服务启动失败: {}", e);
            return Err(e.into());
        }
    };

    let update_handle = thread::spawn(move || {
        while let Ok(update) = update_receiver.recv() {
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
                    error!("[后台错误] {}", e);
                }
                _ => {}
            }
        }
    });

    command_tx.send(smtc_suite::MediaCommand::RequestUpdate)?;

    info!("按 Enter 键即可退出程序。");

    let mut buffer = String::new();
    stdin().lock().read_line(&mut buffer)?;

    command_tx.send(smtc_suite::MediaCommand::Shutdown)?;

    update_handle.join().expect("更新处理线程 join 失败");

    Ok(())
}
