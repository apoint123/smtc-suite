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

use crate::{
    api::{PlaybackStatus, SharedPlayerState},
    utils::await_in_sta,
};

const SMTC_ASYNC_OPERATION_TIMEOUT: TokioDuration = TokioDuration::from_secs(5);
const E_ABORT_HRESULT: windows::core::HRESULT = windows::core::HRESULT(0x8000_4004_u32 as i32);
/// Prevent overly large images from consuming too much memory.
const MAX_COVER_SIZE_BYTES: usize = 20_971_520; // 20 MB

#[allow(clippy::future_not_send)]
pub async fn fetch_cover_data_task(
    thumb_ref: IRandomAccessStreamReference,
    cancel_token: CancellationToken,
) -> WinResult<Option<Vec<u8>>> {
    let start_time = Instant::now();
    let core_task = async {
        let stream_op = thumb_ref.OpenReadAsync()?;
        let stream = await_in_sta(stream_op.into_future()).await?;
        let stream_size = stream.Size()?;

        if stream_size == 0 {
            return Ok(None);
        }
        if stream_size > MAX_COVER_SIZE_BYTES as u64 {
            log::warn!(
                "[Cover Fetcher] Cover data ({stream_size} bytes) exceeded the maximum limit ({MAX_COVER_SIZE_BYTES} bytes) and was discarded."
            );
            return Ok(None);
        }

        let buffer = Buffer::Create(stream_size as u32)?;
        let read_op = stream.ReadAsync(&buffer, buffer.Capacity()?, InputStreamOptions::None)?;
        let bytes_buffer = await_in_sta(read_op.into_future()).await?;

        let reader = DataReader::FromBuffer(&bytes_buffer)?;
        let mut final_bytes = vec![0u8; bytes_buffer.Length()? as usize];
        reader.ReadBytes(&mut final_bytes)?;

        Ok(Some(final_bytes))
    };

    let result = tokio::select! {
        biased;

        () = cancel_token.cancelled() => {
            log::debug!("[Cover Fetcher] The task was canceled.");
            return Err(WinError::from(E_ABORT_HRESULT));
        }

        res = tokio_timeout(SMTC_ASYNC_OPERATION_TIMEOUT, core_task) => {
            res.unwrap_or_else(|_| {
                log::warn!("[Cover Fetcher] The operation to obtain the cover has timed out. (>{SMTC_ASYNC_OPERATION_TIMEOUT:?})。");
                Err(WinError::from(E_ABORT_HRESULT))
            })
        }
    };

    match &result {
        Ok(Some(bytes)) => {
            log::trace!(
                "[Cover Fetcher] {} bytes of cover data were obtained. Total time: {:?}",
                bytes.len(),
                start_time.elapsed()
            );
        }
        Ok(None) => {
            log::debug!(
                "[Cover Fetcher] The task was successfully completed, but there is no valid cover data. Total time: {:?}",
                start_time.elapsed()
            );
        }
        Err(e) => {
            log::warn!(
                "[Cover Fetcher] Task failed: {e:?}, total time taken:{:?}",
                start_time.elapsed()
            );
        }
    }

    result
}

pub async fn progress_timer_task(
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    progress_signal_tx: tokio::sync::mpsc::Sender<()>,
    cancel_token: CancellationToken,
) {
    let mut interval = tokio::time::interval(TokioDuration::from_millis(16));

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
                    log::warn!("[Timer] Unable to send progress update signal, the main event loop may be closed. Exiting task.");
                    break;
                }
            }
        }
    }
}
