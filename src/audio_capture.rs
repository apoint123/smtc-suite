use std::{
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use thiserror::Error;
use tokio::sync::mpsc::Sender as TokioSender;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0},
        Media::{
            Audio::{
                AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
                IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
                WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
            },
            KernelStreaming::WAVE_FORMAT_EXTENSIBLE,
            Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
        },
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            Threading::{
                AvSetMmThreadCharacteristicsW, CreateEventW, INFINITE, SetEvent,
                WaitForMultipleObjects,
            },
        },
    },
    core::HSTRING,
};

use crate::{
    error::{Result, SmtcError},
    worker::InternalUpdate,
};

/// The target sampling rate (in Hz). All captured audio will eventually be
/// resampled to this frequency.
const TARGET_SAMPLE_RATE: u32 = 48000;

/// Target number of channels. 2 for stereo.
const TARGET_CHANNELS: u16 = 2;

/// The recommended reference length of the WASAPI internal buffer (in ms).
const WASAPI_BUFFER_DURATION_MS: u64 = 20;

/// Approximately how often in milliseconds should we attempt to send processed
/// audio packets.
const AUDIO_PACKET_SEND_INTERVAL_MS: u64 = 100;

#[derive(Error, Debug)]
pub enum AudioCaptureError {
    #[error("COM Error: {0}")]
    ComError(#[from] windows::core::Error),
    #[error("Resampler construction error: {0}")]
    ResamplerConstructionError(#[from] rubato::ResamplerConstructionError),
    #[error("Resampler processing error: {0}")]
    ResamplerProcessingError(#[from] rubato::ResampleError),
    #[error("Channel Send Error: {0}")]
    SendError(String),
    #[error("Unsupported audio format: tag={format_tag}, bits={bits_per_sample}")]
    UnsupportedFormat {
        format_tag: u16,
        bits_per_sample: u16,
    },
    #[error("GetMixFormat returned a null pointer")]
    GetMixFormatReturnedNull,
    #[error("Failed to convert byte slice to f32 sample")]
    BytesToSampleConversion,
}

struct AudioClientGuard<'a> {
    client: &'a IAudioClient,
}
impl<'a> AudioClientGuard<'a> {
    fn new(client: &'a IAudioClient) -> Result<Self> {
        unsafe { client.Start()? };
        Ok(Self { client })
    }
}
impl Drop for AudioClientGuard<'_> {
    fn drop(&mut self) {
        if let Err(e) = unsafe { self.client.Stop() } {
            log::warn!("[AudioClientGuard] Failed to stop the audio client: {e:?}");
        }
    }
}

struct WaveFormatGuard(*mut WAVEFORMATEX);
impl Drop for WaveFormatGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(self.0 as *const _)) };
        }
    }
}

// A HANDLE is essentially a pointer and is not thread-safe.
//
// However, the event handler we created here is thread-safe (one thread waits
// while another thread triggers).
unsafe impl Send for EventHandleGuard {}
unsafe impl Sync for EventHandleGuard {}

struct EventHandleGuard(HANDLE);
impl EventHandleGuard {
    fn new(manual_reset: bool, initial_state: bool) -> Result<Self> {
        let handle = unsafe { CreateEventW(None, manual_reset, initial_state, None)? };
        if handle.is_invalid() {
            return Err(SmtcError::AudioCapture(
                windows::core::Error::from_thread().into(),
            ));
        }
        Ok(Self(handle))
    }
}
impl Drop for EventHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            };
        }
    }
}

fn process_and_send_audio_data(
    audio_f32_interleaved: Vec<f32>,
    update_tx: &TokioSender<InternalUpdate>,
    channels_in_audio_data: usize,
    target_channels_for_output: u16,
) -> Result<()> {
    if audio_f32_interleaved.is_empty() {
        return Ok(());
    }
    let final_samples_f32: Vec<f32> = if channels_in_audio_data
        == target_channels_for_output as usize
    {
        audio_f32_interleaved
    } else if channels_in_audio_data > target_channels_for_output as usize
        && target_channels_for_output == 2
    {
        audio_f32_interleaved
            .chunks_exact(channels_in_audio_data)
            .flat_map(|frame| &frame[0..2])
            .copied()
            .collect()
    } else if channels_in_audio_data == 1 && target_channels_for_output == 2 {
        audio_f32_interleaved
            .iter()
            .flat_map(|&sample| [sample, sample])
            .collect()
    } else {
        log::warn!(
            "[Audio Processing] Unsupported channel conversion: from {channels_in_audio_data} channels to {target_channels_for_output} channels. The original channel data will be used directly."
        );
        audio_f32_interleaved
    };
    if !final_samples_f32.is_empty() {
        let audio_data_bytes: Vec<u8> = final_samples_f32
            .iter()
            .flat_map(|&sample_f32| sample_f32.to_le_bytes())
            .collect();
        if update_tx
            .blocking_send(InternalUpdate::AudioDataPacket(audio_data_bytes))
            .is_err()
        {
            let err_msg = "Failed to send audio packet. The channel may be closed.".to_string();
            log::error!("[Audio Processing] {err_msg}");
            return Err(SmtcError::AudioCapture(AudioCaptureError::SendError(
                err_msg,
            )));
        }
    }
    Ok(())
}

pub struct AudioCapturer {
    capture_thread_handle: Option<JoinHandle<()>>,
    stop_event: Option<Arc<EventHandleGuard>>,
}

impl AudioCapturer {
    pub const fn new() -> Self {
        Self {
            capture_thread_handle: None,
            stop_event: None,
        }
    }

    pub fn start_capture(&mut self, update_tx: TokioSender<InternalUpdate>) -> Result<()> {
        if self.capture_thread_handle.is_some() {
            return Ok(());
        }

        let stop_event = Arc::new(EventHandleGuard::new(true, false)?);
        self.stop_event = Some(Arc::clone(&stop_event));

        let thread_builder = thread::Builder::new().name("audio_capture_thread".to_string());
        self.capture_thread_handle = Some(thread_builder.spawn(move || {
            if let Err(e) = Self::run_capture_entrypoint(&stop_event, &update_tx.clone()) {
                log::error!("[Audio capture thread] Capture process encountered error: {e}. Thread will exit.");
                if update_tx
                    .blocking_send(InternalUpdate::AudioCaptureError(e.to_string()))
                    .is_err()
                {
                    log::error!("[Audio capture thread] Failed to send error notification.");
                }
            }
        })?);
        Ok(())
    }

    pub fn stop_capture(&mut self) {
        if let Some(handle) = self.capture_thread_handle.take() {
            if let Some(stop_event) = self.stop_event.take()
                && unsafe { SetEvent(stop_event.0) }.is_err()
            {
                log::error!("[Audio capture thread] Unable to signal a stop event.");
            }
            handle
                .join()
                .expect("Unable to wait for audio capture thread to stop");
        }
    }

    fn run_capture_entrypoint(
        stop_event: &Arc<EventHandleGuard>,
        update_tx: &TokioSender<InternalUpdate>,
    ) -> Result<()> {
        unsafe {
            let task_name_hstring = HSTRING::from("Pro Audio");
            let mut task_index = 0u32;
            if AvSetMmThreadCharacteristicsW(&task_name_hstring, &raw mut task_index).is_err() {
                log::warn!(
                    "[Audio capture thread] Unable to set thread characteristics to 'Pro Audio'."
                );
            }
        }

        let (audio_client, capture_client, wave_format, _format_guard) = Self::init_wasapi()?;

        let wasapi_event = EventHandleGuard::new(false, false)?;
        unsafe { audio_client.SetEventHandle(wasapi_event.0)? };

        let original_channels_usize = wave_format.nChannels as usize;
        let mut resampler = Self::setup_resampler(&wave_format)?;

        let final_accumulated_buffer = {
            let _client_guard = AudioClientGuard::new(&audio_client)?;
            Self::capture_loop(
                stop_event.0,
                wasapi_event.0,
                &capture_client,
                &mut resampler,
                original_channels_usize,
                wave_format.wBitsPerSample,
                update_tx,
            )?
        };

        Self::finalize_stream(
            &mut resampler,
            final_accumulated_buffer,
            original_channels_usize,
            update_tx,
        )?;
        Ok(())
    }

    fn init_wasapi() -> Result<(
        IAudioClient,
        IAudioCaptureClient,
        WAVEFORMATEX,
        WaveFormatGuard,
    )> {
        let device_enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| SmtcError::AudioCapture(e.into()))?;
        let default_device =
            unsafe { device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
                .map_err(|e| SmtcError::AudioCapture(e.into()))?;
        let audio_client: IAudioClient =
            unsafe { default_device.Activate(CLSCTX_INPROC_SERVER, None) }
                .map_err(|e| SmtcError::AudioCapture(e.into()))?;
        let wave_format_ptr = unsafe { audio_client.GetMixFormat() }
            .map_err(|e| SmtcError::AudioCapture(e.into()))?;
        if wave_format_ptr.is_null() {
            return Err(SmtcError::AudioCapture(
                AudioCaptureError::GetMixFormatReturnedNull,
            ));
        }
        let format_guard = WaveFormatGuard(wave_format_ptr);
        let wave_format: WAVEFORMATEX = unsafe { std::ptr::read_unaligned(wave_format_ptr) };
        let is_source_float = (u32::from(wave_format.wFormatTag) == WAVE_FORMAT_IEEE_FLOAT)
            || (u32::from(wave_format.wFormatTag) == WAVE_FORMAT_EXTENSIBLE
                && wave_format.cbSize >= 22
                && unsafe {
                    let wf_ext_ptr = wave_format_ptr as *const WAVEFORMATEXTENSIBLE;
                    let sub_format_ptr = std::ptr::addr_of!((*wf_ext_ptr).SubFormat);
                    let sub_format = std::ptr::read_unaligned(sub_format_ptr);
                    sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
                });
        if !is_source_float || wave_format.wBitsPerSample != 32 {
            return Err(SmtcError::AudioCapture(
                AudioCaptureError::UnsupportedFormat {
                    format_tag: wave_format.wFormatTag,
                    bits_per_sample: wave_format.wBitsPerSample,
                },
            ));
        }
        let hns_buffer_duration: i64 =
            Duration::from_millis(WASAPI_BUFFER_DURATION_MS).as_nanos() as i64 / 100;

        let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

        unsafe {
            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                stream_flags,
                hns_buffer_duration,
                0,
                wave_format_ptr,
                None,
            )
        }
        .map_err(|e| SmtcError::AudioCapture(e.into()))?;
        let capture_client: IAudioCaptureClient =
            unsafe { audio_client.GetService() }.map_err(|e| SmtcError::AudioCapture(e.into()))?;
        Ok((audio_client, capture_client, wave_format, format_guard))
    }

    fn setup_resampler(wave_format: &WAVEFORMATEX) -> Result<Option<SincFixedIn<f32>>> {
        if wave_format.nSamplesPerSec == TARGET_SAMPLE_RATE {
            return Ok(None);
        }
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::Hann,
        };
        let initial_chunk_size = (f64::from(wave_format.nSamplesPerSec)
            * (AUDIO_PACKET_SEND_INTERVAL_MS as f64 / 1000.0))
            as usize;
        let chunk_size_for_resampler = initial_chunk_size.max(params.sinc_len * 2);
        let resampler = SincFixedIn::<f32>::new(
            f64::from(TARGET_SAMPLE_RATE) / f64::from(wave_format.nSamplesPerSec),
            2.0,
            params,
            chunk_size_for_resampler,
            wave_format.nChannels as usize,
        )
        .map_err(|e| SmtcError::AudioCapture(e.into()))?;
        Ok(Some(resampler))
    }

    fn handle_audio_resampling(
        captured_f32_interleaved: &[f32],
        resampler: &mut Option<SincFixedIn<f32>>,
        accumulated_audio_planar: &mut [Vec<f32>],
        original_channels: usize,
    ) -> Result<Vec<f32>> {
        let mut data_to_send_interleaved = Vec::new();
        if let Some(rs) = resampler {
            for (sample_idx, &sample) in captured_f32_interleaved.iter().enumerate() {
                accumulated_audio_planar[sample_idx % original_channels].push(sample);
            }
            let chunk_size = rs.input_frames_next();
            while accumulated_audio_planar
                .iter()
                .all(|ch| ch.len() >= chunk_size)
            {
                let resampler_input: Vec<&[f32]> = accumulated_audio_planar
                    .iter()
                    .map(|ch| &ch[..chunk_size])
                    .collect();
                let mut resampler_output =
                    vec![vec![0.0; rs.output_frames_max()]; original_channels];
                let (_, frames_written) = rs
                    .process_into_buffer(
                        &resampler_input,
                        &mut resampler_output
                            .iter_mut()
                            .map(Vec::as_mut_slice)
                            .collect::<Vec<_>>(),
                        None,
                    )
                    .map_err(|e| SmtcError::AudioCapture(e.into()))?;
                for frame_idx in 0..frames_written {
                    for channel_data in resampler_output.iter().take(original_channels) {
                        data_to_send_interleaved.push(channel_data[frame_idx]);
                    }
                }
                for channel_buffer in accumulated_audio_planar.iter_mut() {
                    channel_buffer.drain(0..chunk_size);
                }
            }
        } else {
            data_to_send_interleaved.extend_from_slice(captured_f32_interleaved);
        }
        Ok(data_to_send_interleaved)
    }

    fn capture_loop(
        stop_handle: HANDLE,
        wasapi_handle: HANDLE,
        capture_client: &IAudioCaptureClient,
        resampler: &mut Option<SincFixedIn<f32>>,
        original_channels: usize,
        original_bits_per_sample: u16,
        update_tx: &TokioSender<InternalUpdate>,
    ) -> Result<Vec<Vec<f32>>> {
        let mut captured_f32_interleaved: Vec<f32> = Vec::with_capacity(4096);
        let mut data_to_send_interleaved: Vec<f32> = Vec::with_capacity(4096);
        let mut accumulated_audio_planar: Vec<Vec<f32>> =
            vec![Vec::with_capacity(4096); original_channels];

        let handles = [stop_handle, wasapi_handle];

        loop {
            let wait_result: WAIT_EVENT =
                unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };

            match wait_result {
                WAIT_OBJECT_0 => {
                    log::debug!(
                        "[Audio capture thread] Received stop signal, exit the capture loop."
                    );
                    break;
                }
                wait_event if wait_event.0 == WAIT_OBJECT_0.0 + 1 => loop {
                    let packet_length_frames = unsafe { capture_client.GetNextPacketSize() }?;
                    if packet_length_frames == 0 {
                        break;
                    }

                    {
                        let (mut p_data, mut num_frames_captured, mut dw_flags) =
                            (std::ptr::null_mut(), 0, 0);
                        unsafe {
                            capture_client.GetBuffer(
                                &raw mut p_data,
                                &raw mut num_frames_captured,
                                &raw mut dw_flags,
                                None,
                                None,
                            )?;
                        };

                        struct BufferGuard<'a> {
                            client: &'a IAudioCaptureClient,
                            frames: u32,
                        }
                        impl Drop for BufferGuard<'_> {
                            fn drop(&mut self) {
                                unsafe {
                                    let _ = self.client.ReleaseBuffer(self.frames);
                                }
                            }
                        }
                        let _guard = BufferGuard {
                            client: capture_client,
                            frames: num_frames_captured,
                        };

                        if dw_flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                            continue;
                        }

                        if num_frames_captured > 0 && !p_data.is_null() {
                            captured_f32_interleaved.clear();
                            let bytes_per_frame =
                                original_channels * (original_bits_per_sample / 8) as usize;
                            let captured_bytes_slice = unsafe {
                                std::slice::from_raw_parts(
                                    p_data,
                                    num_frames_captured as usize * bytes_per_frame,
                                )
                            };
                            for sample_bytes in captured_bytes_slice
                                .chunks_exact((original_bits_per_sample / 8) as usize)
                            {
                                captured_f32_interleaved.push(f32::from_le_bytes(
                                    sample_bytes.try_into().map_err(|_| {
                                        SmtcError::AudioCapture(
                                            AudioCaptureError::BytesToSampleConversion,
                                        )
                                    })?,
                                ));
                            }
                            let processed_data = Self::handle_audio_resampling(
                                &captured_f32_interleaved,
                                resampler,
                                &mut accumulated_audio_planar,
                                original_channels,
                            )?;
                            data_to_send_interleaved.extend(processed_data);
                        }
                    }
                    if !data_to_send_interleaved.is_empty() {
                        let data_to_send = std::mem::take(&mut data_to_send_interleaved);
                        process_and_send_audio_data(
                            data_to_send,
                            update_tx,
                            original_channels,
                            TARGET_CHANNELS,
                        )?;
                    }
                },
                _ => {
                    let err = windows::core::Error::from_thread();
                    log::error!("[Audio capture thread] WaitForMultipleObjects failed: {err:?}");
                    return Err(SmtcError::AudioCapture(err.into()));
                }
            }
        }
        Ok(accumulated_audio_planar)
    }

    fn finalize_stream(
        resampler: &mut Option<SincFixedIn<f32>>,
        mut accumulated_audio_planar: Vec<Vec<f32>>,
        original_channels: usize,
        update_tx: &TokioSender<InternalUpdate>,
    ) -> Result<()> {
        if let Some(rs) = resampler {
            let needed_frames = rs.input_frames_next();
            if accumulated_audio_planar.iter().any(|b| !b.is_empty()) {
                for channel_buffer in &mut accumulated_audio_planar {
                    let padding_needed = needed_frames.saturating_sub(channel_buffer.len());
                    channel_buffer.extend(std::iter::repeat_n(0.0f32, padding_needed));
                }
                let mut output_buffer = vec![vec![0.0; rs.output_frames_max()]; original_channels];
                let input_slices: Vec<&[f32]> =
                    accumulated_audio_planar.iter().map(Vec::as_slice).collect();
                let (_, frames_written) = rs
                    .process_into_buffer(
                        &input_slices,
                        &mut output_buffer
                            .iter_mut()
                            .map(Vec::as_mut_slice)
                            .collect::<Vec<_>>(),
                        None,
                    )
                    .map_err(|e| SmtcError::AudioCapture(e.into()))?;
                if frames_written > 0 {
                    let mut last_data = Vec::with_capacity(frames_written * original_channels);
                    for frame_idx in 0..frames_written {
                        for channel_data in &output_buffer {
                            last_data.push(channel_data[frame_idx]);
                        }
                    }
                    process_and_send_audio_data(
                        last_data,
                        update_tx,
                        original_channels,
                        TARGET_CHANNELS,
                    )?;
                }
            }
        }
        Ok(())
    }
}

impl Drop for AudioCapturer {
    fn drop(&mut self) {
        self.stop_capture();
    }
}
