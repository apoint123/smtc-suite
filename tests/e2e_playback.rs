use serde::{Deserialize, Serialize};
use smtc_suite::{MediaCommand, MediaManager, MediaUpdate, SmtcControlCommand};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::timeout;
use windows::core::{IInspectable, Interface};
use windows::{
    Foundation::TypedEventHandler,
    Media::{
        MediaPlaybackStatus, SystemMediaTransportControls, SystemMediaTransportControlsButton,
        SystemMediaTransportControlsButtonPressedEventArgs,
        SystemMediaTransportControlsTimelineProperties,
    },
    Storage::Streams::{DataWriter, InMemoryRandomAccessStream, RandomAccessStreamReference},
    Win32::{
        Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            WinRT::{
                ISystemMediaTransportControlsInterop, RO_INIT_SINGLETHREADED, RoInitialize,
                RoUninitialize,
            },
        },
        UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, RegisterClassW, WNDCLASSW,
            WS_OVERLAPPEDWINDOW,
        },
    },
    core::{HSTRING, PCWSTR, Ref, Result as WinResult},
};

static E2E_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerializableStatus {
    Closed,
    Changing,
    Stopped,
    Playing,
    Paused,
}

impl From<SerializableStatus> for MediaPlaybackStatus {
    fn from(status: SerializableStatus) -> Self {
        match status {
            SerializableStatus::Closed => Self::Closed,
            SerializableStatus::Changing => Self::Changing,
            SerializableStatus::Stopped => Self::Stopped,
            SerializableStatus::Playing => Self::Playing,
            SerializableStatus::Paused => Self::Paused,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "command")]
enum PlayerCommand {
    SetSong {
        title: String,
        artist: String,
        album: String,
    },
    SetTimeline {
        position_ms: u64,
        duration_ms: u64,
    },
    SetStatus(SerializableStatus),
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "event", content = "payload")]
enum PlayerEvent {
    ButtonPressed { button: String },
}

struct RoGuard(());
impl RoGuard {
    fn new() -> WinResult<Self> {
        unsafe { RoInitialize(RO_INIT_SINGLETHREADED)? };
        Ok(Self(()))
    }
}
impl Drop for RoGuard {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

unsafe extern "system" fn dummy_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn create_dummy_thumbnail() -> WinResult<RandomAccessStreamReference> {
    let stream = InMemoryRandomAccessStream::new()?;
    let writer = DataWriter::CreateDataWriter(&stream)?;
    let dummy_data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
    writer.WriteBytes(&dummy_data)?;
    writer.StoreAsync()?.get()?;
    writer.FlushAsync()?.get()?;
    RandomAccessStreamReference::CreateFromStream(&stream)
}

fn setup_smtc() -> WinResult<SystemMediaTransportControls> {
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None)?;
        let class_name = HSTRING::from("DummyTestWindowClass");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dummy_wnd_proc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&raw const wc) == 0 {
            let last_error = GetLastError();
            if last_error != ERROR_CLASS_ALREADY_EXISTS {
                return Err(last_error.into());
            }
        }
        #[allow(clippy::default_trait_access)]
        CreateWindowExW(
            Default::default(),
            &class_name,
            &HSTRING::from("Dummy Test Window"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };
    let interop = windows::core::factory::<
        SystemMediaTransportControls,
        ISystemMediaTransportControlsInterop,
    >()?;
    let smtc: SystemMediaTransportControls =
        unsafe { interop.GetForWindow::<IInspectable>(hwnd)? }.cast()?;
    smtc.SetIsEnabled(true)?;
    smtc.SetIsPlayEnabled(true)?;
    smtc.SetIsPauseEnabled(true)?;
    smtc.SetIsNextEnabled(true)?;
    smtc.SetIsPreviousEnabled(true)?;
    Ok(smtc)
}

fn button_enum_to_string(button: SystemMediaTransportControlsButton) -> String {
    match button {
        SystemMediaTransportControlsButton::Play => "Play".to_string(),
        SystemMediaTransportControlsButton::Pause => "Pause".to_string(),
        SystemMediaTransportControlsButton::Next => "Next".to_string(),
        SystemMediaTransportControlsButton::Previous => "Previous".to_string(),
        SystemMediaTransportControlsButton::Stop => "Stop".to_string(),
        _ => format!("Other({button:?})"),
    }
}

async fn virtual_player_main_loop(listener: TcpListener, event_tx: mpsc::Sender<PlayerEvent>) {
    let _guard = RoGuard::new().expect("Player failed to initialize COM");
    let smtc = setup_smtc().expect("Player failed to setup SMTC");

    let (stream, _) = listener.accept().await.unwrap();
    let (reader, _) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    let button_event_tx = event_tx.clone();
    smtc.ButtonPressed(&TypedEventHandler::new(
        move |_, args: Ref<SystemMediaTransportControlsButtonPressedEventArgs>| -> WinResult<()> {
            if let Some(ref event_args) = *args {
                let button_str = button_enum_to_string(event_args.Button()?);
                let event = PlayerEvent::ButtonPressed { button: button_str };
                let _ = button_event_tx.try_send(event);
            }
            Ok(())
        },
    ))
    .unwrap();

    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap() == 0 {
            break;
        }

        let command: PlayerCommand = serde_json::from_str(&line).unwrap();
        match command {
            PlayerCommand::SetSong {
                title,
                artist,
                album,
            } => {
                let updater = smtc.DisplayUpdater().unwrap();
                updater
                    .SetType(windows::Media::MediaPlaybackType::Music)
                    .unwrap();
                let props = updater.MusicProperties().unwrap();
                props.SetTitle(&HSTRING::from(title)).unwrap();
                props.SetArtist(&HSTRING::from(artist)).unwrap();
                props.SetAlbumTitle(&HSTRING::from(album)).unwrap();
                updater
                    .SetThumbnail(&create_dummy_thumbnail().unwrap())
                    .unwrap();
                updater.Update().unwrap();
            }
            PlayerCommand::SetTimeline {
                position_ms,
                duration_ms,
            } => {
                let timeline_props = SystemMediaTransportControlsTimelineProperties::new().unwrap();
                timeline_props
                    .SetStartTime(Duration::from_secs(0).into())
                    .unwrap();
                timeline_props
                    .SetEndTime(Duration::from_millis(duration_ms).into())
                    .unwrap();
                timeline_props
                    .SetPosition(Duration::from_millis(position_ms).into())
                    .unwrap();
                smtc.UpdateTimelineProperties(&timeline_props).unwrap();
            }
            PlayerCommand::SetStatus(status) => {
                smtc.SetPlaybackStatus(status.into()).unwrap();
            }
            PlayerCommand::Shutdown => {
                break;
            }
        }
    }
}

struct TestHarness {
    player_writer: WriteHalf<TcpStream>,
    player_event_rx: mpsc::Receiver<PlayerEvent>,
    smtc_controller: smtc_suite::MediaController,
    smtc_update_rx: mpsc::Receiver<MediaUpdate>,
}

impl TestHarness {
    async fn setup() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (player_event_tx, player_event_rx) = mpsc::channel(32);

        tokio::spawn(virtual_player_main_loop(listener, player_event_tx));

        let stream = TcpStream::connect(addr).await.unwrap();
        let (_reader, writer) = tokio::io::split(stream);

        let (controller, update_rx) = MediaManager::start().unwrap();

        Self {
            player_writer: writer,
            player_event_rx,
            smtc_controller: controller,
            smtc_update_rx: update_rx,
        }
    }

    async fn send_player_cmd(&mut self, cmd: &PlayerCommand) {
        let json = serde_json::to_string(cmd).unwrap() + "\n";
        self.player_writer.write_all(json.as_bytes()).await.unwrap();
    }

    async fn shutdown(mut self) {
        self.send_player_cmd(&PlayerCommand::Shutdown).await;
        self.smtc_controller.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn test_e2e_receive_track_update() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("trace"))
        .try_init();

    let _lock = E2E_TEST_MUTEX.lock().await;
    let _guard = RoGuard::new().expect("Test runner failed to initialize COM");
    let mut harness = TestHarness::setup().await;

    harness
        .send_player_cmd(&PlayerCommand::SetSong {
            title: "Solaris".into(),
            artist: "Stellardrone".into(),
            album: "Light Years".into(),
        })
        .await;

    let info_after_song_set = timeout(Duration::from_secs(5), async {
        loop {
            let update = harness.smtc_update_rx.recv().await.unwrap();
            if let MediaUpdate::TrackChanged(info) = update
                && info.title.as_deref() == Some("Solaris")
            {
                break info;
            }
        }
    })
    .await
    .expect("Timeout waiting for track title to update");

    assert_eq!(info_after_song_set.title.as_deref(), Some("Solaris"));
    assert_eq!(info_after_song_set.artist.as_deref(), Some("Stellardrone"));

    tokio::time::sleep(Duration::from_millis(100)).await;
    harness
        .send_player_cmd(&PlayerCommand::SetStatus(SerializableStatus::Playing))
        .await;

    let info_after_status_set = timeout(Duration::from_secs(5), async {
        loop {
            let update = harness.smtc_update_rx.recv().await.unwrap();
            if let MediaUpdate::TrackChanged(info) = update
                && info.playback_status == Some(smtc_suite::PlaybackStatus::Playing)
            {
                break info;
            }
        }
    })
    .await
    .expect("Timeout waiting for playback status to update to Playing");

    assert_eq!(info_after_status_set.title.as_deref(), Some("Solaris"));
    assert_eq!(
        info_after_status_set.playback_status,
        Some(smtc_suite::PlaybackStatus::Playing)
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn test_e2e_send_pause_command() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("trace"))
        .try_init();

    let _lock = E2E_TEST_MUTEX.lock().await;

    let _guard = RoGuard::new().expect("Test runner failed to initialize COM");
    let mut harness = TestHarness::setup().await;

    harness
        .send_player_cmd(&PlayerCommand::SetStatus(SerializableStatus::Playing))
        .await;

    let player_session_id = timeout(Duration::from_secs(5), async {
        loop {
            let update = harness.smtc_update_rx.recv().await.unwrap();
            if let MediaUpdate::SessionsChanged(sessions) = update
                && let Some(session) = sessions
                    .iter()
                    .find(|s| s.session_id.contains("e2e_playback"))
            {
                break session.session_id.clone();
            }
        }
    })
    .await
    .expect("Timeout waiting for virtual player session to appear");

    harness
        .smtc_controller
        .command_tx
        .send(MediaCommand::SelectSession(player_session_id))
        .await
        .unwrap();

    harness
        .smtc_controller
        .command_tx
        .send(MediaCommand::Control(SmtcControlCommand::Pause))
        .await
        .unwrap();

    let event = timeout(Duration::from_secs(3), harness.player_event_rx.recv())
        .await
        .expect("Timeout waiting for ButtonPressed event from virtual player")
        .unwrap();

    assert_eq!(
        event,
        PlayerEvent::ButtonPressed {
            button: "Pause".to_string()
        }
    );

    // 6. 清理
    harness.shutdown().await;
}
