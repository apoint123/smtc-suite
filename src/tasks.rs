use std::{future::IntoFuture, sync::Arc, time::Instant};

use tokio::{
    sync::Mutex as TokioMutex,
    time::{Duration as TokioDuration, timeout as tokio_timeout},
};
use tokio_util::sync::CancellationToken;
use windows::{
    Storage::Streams::{Buffer, DataReader, IRandomAccessStreamReference, InputStreamOptions},
    core::{Error as WinError, Result as WinResult},
};
use windows_core::Interface;
use windows_future::IAsyncInfo;

use crate::api::{PlaybackStatus, SharedPlayerState};

/// SMTC 异步操作的通用超时时长。
const SMTC_ASYNC_OPERATION_TIMEOUT: TokioDuration = TokioDuration::from_secs(5);
/// Windows API 操作被中止时返回的 HRESULT 错误码 (`E_ABORT`)。
const E_ABORT_HRESULT: windows::core::HRESULT = windows::core::HRESULT(0x8000_4004_u32 as i32);
/// 允许获取的封面图片的最大字节数，防止过大的图片消耗过多内存。
const MAX_COVER_SIZE_BYTES: usize = 20_971_520; // 20 MB

/// 使用超时来执行一个 `WinRT` 异步操作。
/// 如果超时，会尝试取消该操作。
async fn run_winrt_op_with_timeout<F, T>(operation: F) -> WinResult<T>
where
    T: windows::core::RuntimeType + 'static,
    T::Default: 'static,
    F: IntoFuture<Output = WinResult<T>> + Interface + Clone,
{
    match tokio_timeout(
        SMTC_ASYNC_OPERATION_TIMEOUT,
        operation.clone().into_future(),
    )
    .await
    {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            log::warn!("WinRT 异步操作超时 (>{SMTC_ASYNC_OPERATION_TIMEOUT:?}).");

            if let Ok(async_info) = operation.cast::<IAsyncInfo>() {
                if let Err(e) = async_info.Cancel() {
                    log::warn!("取消 WinRT 异步操作失败: {e:?}");
                }
            } else {
                log::warn!("无法将异步操作转换为 IAsyncInfo 来执行取消操作。");
            }

            Err(WinError::from(E_ABORT_HRESULT))
        }
    }
}

/// 从 SMTC 会话中获取封面图片数据。
pub async fn fetch_cover_data_task(
    thumb_ref: IRandomAccessStreamReference,
    cancel_token: CancellationToken,
) -> WinResult<Option<Vec<u8>>> {
    let start_time = Instant::now();
    log::trace!("[Cover Fetcher] 正在获取封面数据...");

    let result = tokio::select! {
        biased;
        () = cancel_token.cancelled() => {
            log::debug!("[Cover Fetcher] 任务被取消。");
            return Err(WinError::from(E_ABORT_HRESULT));
        }
        res = async {
            let stream = run_winrt_op_with_timeout(thumb_ref.OpenReadAsync()?).await?;
            log::trace!("[Cover Fetcher] 成功获取到封面流 (IRandomAccessStreamWithContentType)。");

            let stream_size = stream.Size()?;

            if stream_size == 0 {
                log::warn!("[Cover Fetcher] 未能获取到封面（流大小为0）。");
                return Ok(None);
            }
            if stream_size > MAX_COVER_SIZE_BYTES as u64 {
                log::warn!(
                    "[Cover Fetcher] 封面数据 ({stream_size} 字节) 超出最大限制 ({MAX_COVER_SIZE_BYTES} 字节)，已丢弃。"
                );
                return Ok(None);
            }

            let buffer = Buffer::Create(stream_size as u32)?;

            let read_operation = stream.ReadAsync(&buffer, buffer.Capacity()?, InputStreamOptions::None)?;
            let bytes_buffer = run_winrt_op_with_timeout(read_operation).await?;

            let reader = DataReader::FromBuffer(&bytes_buffer)?;
            let mut bytes = vec![0u8; bytes_buffer.Length()? as usize];
            reader.ReadBytes(&mut bytes)?;
            Ok(Some(bytes))
        } => res
    };

    match &result {
        Ok(Some(bytes)) => {
            log::trace!(
                "[Cover Fetcher] 获取到 {} 字节的封面数据。总耗时: {:?}",
                bytes.len(),
                start_time.elapsed()
            );
        }
        Ok(None) => {
            log::debug!(
                "[Cover Fetcher] 任务成功完成，但无有效封面数据。总耗时: {:?}",
                start_time.elapsed()
            );
        }
        Err(e) => {
            log::warn!(
                "[Cover Fetcher] 任务失败: {e:?}, 总耗时: {:?}",
                start_time.elapsed()
            );
        }
    }

    result
}

/// 一个任务，用于定期计算并发送估算的播放进度。
pub async fn progress_timer_task(
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    progress_signal_tx: tokio::sync::mpsc::Sender<()>,
    cancel_token: CancellationToken,
) {
    log::trace!("[Timer] 计时器任务已启动。");
    let mut interval = tokio::time::interval(TokioDuration::from_millis(100));

    loop {
        tokio::select! {
            () = cancel_token.cancelled() => {
                break;
            }
            _ = interval.tick() => {
                let state_guard = player_state_arc.lock().await;

                if state_guard.playback_status == PlaybackStatus::Playing
                    && !state_guard.is_waiting_for_initial_update
                    && progress_signal_tx.send(()).await.is_err()
                {
                    log::warn!("[Timer] 无法发送进度更新信号，主事件循环可能已关闭。任务退出。");
                    break;
                }
            }
        }
    }
    log::trace!("[Timer] 计时器任务已结束。");
}
