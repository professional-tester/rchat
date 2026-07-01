use crate::{
    bridge::{TuiEvent, TuiEventSink},
    media::{
        DecodedRgbaFrame, LatestFrameSlot, ProtocolRequest, ProtocolResponse, ProtocolWorker,
        ScreenFrameDecoder,
    },
    smoke::SmokeFrameGenerator,
};
use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use ratatui_image::{picker::ProtocolType, Image};
use rchat_core::{
    app_state::{BroadcastPhase, BroadcastState},
    events::CoreEvent,
    live::broadcast::protocol::BroadcastFrameEvent,
    live::video::codec::{i420_to_rgba, VideoProfile, Vp8VideoDecoder, Vp8VideoEncoder},
    network::command::NetworkCommand,
    runtime,
    AppState, NetworkState,
};
use rchat_screen_capture::{ScreenCaptureConfig, ScreenCaptureProfile, ScreenCaptureSession};
use std::{io, sync::Arc, time::Duration};
use tokio::sync::mpsc;

const TUI_EVENT_BUFFER: usize = 512;
const MEDIA_PRESENT_INTERVAL: Duration = Duration::from_millis(100);
const DEFAULT_SMOKE_FPS: u32 = 10;
const DEFAULT_SMOKE_SECONDS: u64 = 20;
const DEFAULT_LOCAL_SCREEN_SECONDS: u64 = 30;
const SMOKE_WIDTH: u32 = 640;
const SMOKE_HEIGHT: u32 = 360;

#[derive(Debug, Parser)]
#[command(name = "rchat-tui")]
#[command(about = "Terminal RChat client media spike")]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    MediaSmoke {
        #[arg(long, default_value_t = DEFAULT_SMOKE_FPS)]
        fps: u32,
        #[arg(long, default_value_t = DEFAULT_SMOKE_SECONDS)]
        seconds: u64,
    },
    LocalScreenSmoke {
        #[arg(long, value_parser = parse_screen_profile, default_value = "720p15")]
        profile: ScreenCaptureProfile,
        #[arg(long, value_enum, default_value = "vp8")]
        path: LocalScreenPath,
        #[arg(long, default_value_t = DEFAULT_LOCAL_SCREEN_SECONDS)]
        seconds: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LocalScreenPath {
    Direct,
    Vp8,
}

impl LocalScreenPath {
    fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Vp8 => "vp8",
        }
    }
}

fn parse_screen_profile(value: &str) -> std::result::Result<ScreenCaptureProfile, String> {
    ScreenCaptureProfile::from_label(value).ok_or_else(|| {
        "expected one of: 480p15, 480p30, 720p15, 720p30".to_string()
    })
}

fn video_profile_for_screen_profile(profile: ScreenCaptureProfile) -> VideoProfile {
    match profile {
        ScreenCaptureProfile::P480F15 | ScreenCaptureProfile::P480F30 => VideoProfile::P480,
        ScreenCaptureProfile::P720F15 | ScreenCaptureProfile::P720F30 => VideoProfile::P720,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_screen_smoke_defaults() {
        let cli = Cli::try_parse_from(["rchat-tui", "local-screen-smoke"]).unwrap();

        let Some(Command::LocalScreenSmoke {
            profile,
            path,
            seconds,
        }) = cli.command
        else {
            panic!("expected local screen smoke command");
        };

        assert_eq!(profile, rchat_screen_capture::ScreenCaptureProfile::P720F15);
        assert_eq!(path, LocalScreenPath::Vp8);
        assert_eq!(seconds, 30);
    }

    #[test]
    fn parses_local_screen_smoke_options() {
        let cli = Cli::try_parse_from([
            "rchat-tui",
            "local-screen-smoke",
            "--profile",
            "480p30",
            "--path",
            "direct",
            "--seconds",
            "5",
        ])
        .unwrap();

        let Some(Command::LocalScreenSmoke {
            profile,
            path,
            seconds,
        }) = cli.command
        else {
            panic!("expected local screen smoke command");
        };

        assert_eq!(profile, rchat_screen_capture::ScreenCaptureProfile::P480F30);
        assert_eq!(path, LocalScreenPath::Direct);
        assert_eq!(seconds, 5);
    }

    fn synthetic_i420(width: u32, height: u32) -> Vec<u8> {
        let y_len = (width * height) as usize;
        let uv_len = y_len / 4;
        let mut data = vec![96_u8; y_len];
        data.extend(std::iter::repeat(128_u8).take(uv_len));
        data.extend(std::iter::repeat(128_u8).take(uv_len));
        data
    }

    fn screen_frame(width: u32, height: u32) -> rchat_screen_capture::I420ScreenFrame {
        rchat_screen_capture::I420ScreenFrame {
            timestamp_us: 123,
            width,
            height,
            data: synthetic_i420(width, height),
        }
    }

    #[test]
    fn local_screen_direct_path_converts_i420_to_rgba() {
        let mut encoder = None;
        let mut decoder = None;

        let (frame, encoded_count) = local_screen_frame_to_rgba(
            LocalScreenPath::Direct,
            ScreenCaptureProfile::P480F15,
            &mut encoder,
            &mut decoder,
            7,
            screen_frame(16, 16),
        )
        .unwrap()
        .unwrap();

        assert_eq!(frame.seq, 7);
        assert_eq!(frame.width, 16);
        assert_eq!(frame.height, 16);
        assert_eq!(frame.rgba.len(), 16 * 16 * 4);
        assert_eq!(encoded_count, 0);
    }

    #[test]
    fn local_screen_vp8_path_round_trips_i420_to_rgba() {
        let mut encoder = Some(
            Vp8VideoEncoder::new_with_dimensions(
                video_profile_for_screen_profile(ScreenCaptureProfile::P480F15),
                16,
                16,
            )
            .unwrap(),
        );
        let mut decoder = Some(Vp8VideoDecoder::new().unwrap());

        let (frame, encoded_count) = local_screen_frame_to_rgba(
            LocalScreenPath::Vp8,
            ScreenCaptureProfile::P480F15,
            &mut encoder,
            &mut decoder,
            0,
            screen_frame(16, 16),
        )
        .unwrap()
        .unwrap();

        assert_eq!(frame.seq, 0);
        assert_eq!(frame.width, 16);
        assert_eq!(frame.height, 16);
        assert_eq!(frame.rgba.len(), 16 * 16 * 4);
        assert!(encoded_count > 0);
    }
}

pub fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .try_init()
        .ok();

    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new().context("failed to start tokio runtime")?;
    runtime.block_on(async move {
        match cli.command {
            Some(Command::MediaSmoke { fps, seconds }) => run_media_smoke(fps, seconds).await,
            Some(Command::LocalScreenSmoke {
                profile,
                path,
                seconds,
            }) => run_local_screen_smoke(profile, path, seconds).await,
            None => run_interactive().await,
        }
    })
}

async fn run_interactive() -> Result<()> {
    let app_state = create_unlocked_app_state().await?;
    let (event_sink, mut event_rx) = TuiEventSink::channel(TUI_EVENT_BUFFER);
    let network_state = rchat_core::network::start(app_state, Arc::new(event_sink.clone()))
        .await
        .context("failed to start rchat-core network")?;

    let mut terminal = TerminalSession::enter()?;
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    let protocol_type = picker.protocol_type();
    let kitty_available = protocol_type == ProtocolType::Kitty;
    let protocol_worker = kitty_available.then(|| ProtocolWorker::spawn(picker));
    let mut state = UiState::new(protocol_type, event_sink);
    let mut pending_frames = LatestFrameSlot::<BroadcastFrameEvent>::default();
    let mut decoder = ScreenFrameDecoder::default();
    let mut last_media_present = std::time::Instant::now() - MEDIA_PRESENT_INTERVAL;

    loop {
        drain_core_events(
            &mut event_rx,
            &mut state,
            &mut pending_frames,
            &mut decoder,
        );

        if let Some(worker) = protocol_worker.as_ref() {
            if last_media_present.elapsed() >= MEDIA_PRESENT_INTERVAL {
                if let Some(event) = pending_frames.take() {
                    if let Some(frame) = decoder.decode_event(&event) {
                        state.decoded_frames = state.decoded_frames.saturating_add(1);
                        worker.request(ProtocolRequest {
                            frame,
                            size: state.media_size,
                        });
                    }
                }
                last_media_present = std::time::Instant::now();
            }

            if let Some(response) = worker.try_recv_latest() {
                match response {
                    Ok(protocol) => {
                        state.last_protocol_seq = Some(protocol.seq);
                        state.protocol = Some(protocol);
                        state.media_error = None;
                    }
                    Err(error) => state.media_error = Some(error),
                }
            }
        }

        state.pending_frame_drops = pending_frames.dropped_frames();
        state.decoder_dropped_delta = decoder.dropped_delta_before_keyframe();
        state.decoder_errors = decoder.decode_errors();

        while event::poll(Duration::from_millis(1))? {
            let CrosstermEvent::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('?') => state.show_help = !state.show_help,
                KeyCode::Char('a') => {
                    if let Some(session_id) = state.incoming_session_id.clone() {
                        send_network_command(
                            &network_state,
                            NetworkCommand::AcceptScreenBroadcast { session_id },
                        )
                        .await?;
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(session_id) = state.incoming_session_id.clone() {
                        send_network_command(
                            &network_state,
                            NetworkCommand::RejectScreenBroadcast { session_id },
                        )
                        .await?;
                    }
                }
                KeyCode::Char('e') => {
                    if let Some(session_id) = state.active_session_id.clone() {
                        send_network_command(
                            &network_state,
                            NetworkCommand::EndScreenBroadcast { session_id },
                        )
                        .await?;
                    }
                }
                _ => {}
            }
        }

        terminal.draw(|frame| render_ui(frame, &mut state, kitty_available))?;
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
}

async fn run_media_smoke(fps: u32, seconds: u64) -> Result<()> {
    if fps == 0 {
        return Err(anyhow!("fps must be greater than zero"));
    }

    let mut terminal = TerminalSession::enter()?;
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    let protocol_type = picker.protocol_type();
    let kitty_available = protocol_type == ProtocolType::Kitty;
    let protocol_worker = kitty_available.then(|| ProtocolWorker::spawn(picker));
    let mut state = UiState::new(protocol_type, TuiEventSink::channel(1).0);
    state.status = "media smoke".to_string();

    let mut generator = SmokeFrameGenerator::new(SMOKE_WIDTH, SMOKE_HEIGHT);
    let frame_interval = Duration::from_millis(1_000 / u64::from(fps));
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    let mut next_frame_at = std::time::Instant::now();

    while std::time::Instant::now() < deadline {
        if let Some(worker) = protocol_worker.as_ref() {
            if std::time::Instant::now() >= next_frame_at {
                worker.request(ProtocolRequest {
                    frame: generator.next_frame(),
                    size: state.media_size,
                });
                next_frame_at += frame_interval;
            }

            if let Some(response) = worker.try_recv_latest() {
                match response {
                    Ok(protocol) => {
                        state.decoded_frames = state.decoded_frames.saturating_add(1);
                        state.last_protocol_seq = Some(protocol.seq);
                        state.protocol = Some(protocol);
                        state.media_error = None;
                    }
                    Err(error) => state.media_error = Some(error),
                }
            }
        }

        while event::poll(Duration::from_millis(1))? {
            let CrosstermEvent::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('q') {
                return Ok(());
            }
        }

        terminal.draw(|frame| render_ui(frame, &mut state, kitty_available))?;
        tokio::time::sleep(Duration::from_millis(16)).await;
    }

    Ok(())
}

async fn run_local_screen_smoke(
    profile: ScreenCaptureProfile,
    path: LocalScreenPath,
    seconds: u64,
) -> Result<()> {
    let config = ScreenCaptureConfig::primary_display_for_profile(profile);
    let mut capture_session = ScreenCaptureSession::start(config)
        .await
        .context("failed to start local screen capture")?;
    let capture_info = capture_session.info().clone();

    let mut terminal = TerminalSession::enter()?;
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    let protocol_type = picker.protocol_type();
    let kitty_available = protocol_type == ProtocolType::Kitty;
    let protocol_worker = kitty_available.then(|| ProtocolWorker::spawn(picker));
    let mut state = UiState::new(protocol_type, TuiEventSink::channel(1).0);
    state.status = format!(
        "local screen smoke {} {}",
        profile.label(),
        path.label()
    );
    state.active_session_id = Some(format!(
        "{} {}x{}@{}",
        capture_info.backend.label(),
        capture_info.format.width,
        capture_info.format.height,
        capture_info.format.fps
    ));

    let mut encoder = (path == LocalScreenPath::Vp8)
        .then(|| {
            let (width, height) = profile.dimensions();
            Vp8VideoEncoder::new_with_dimensions(
                video_profile_for_screen_profile(profile),
                width,
                height,
            )
        })
        .transpose()
        .map_err(|error| anyhow!("failed to start local VP8 encoder: {error}"))?;
    let mut decoder = (path == LocalScreenPath::Vp8)
        .then(Vp8VideoDecoder::new)
        .transpose()
        .map_err(|error| anyhow!("failed to start local VP8 decoder: {error}"))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    let mut next_seq = 0_u32;
    let mut encoded_frames = 0_u64;

    while std::time::Instant::now() < deadline {
        if let Some(worker) = protocol_worker.as_ref() {
            if let Some(frame) = capture_session.try_recv_latest_i420() {
                state.received_frames = state.received_frames.saturating_add(1);
                match local_screen_frame_to_rgba(
                    path,
                    profile,
                    &mut encoder,
                    &mut decoder,
                    next_seq,
                    frame,
                ) {
                    Ok(Some((frame, encoded_count))) => {
                        encoded_frames = encoded_frames.saturating_add(encoded_count);
                        state.decoded_frames = state.decoded_frames.saturating_add(1);
                        worker.request(ProtocolRequest {
                            frame,
                            size: state.media_size,
                        });
                        next_seq = next_seq.wrapping_add(1);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        state.decoder_errors = state.decoder_errors.saturating_add(1);
                        state.media_error = Some(error.to_string());
                    }
                }
            }

            if let Some(response) = worker.try_recv_latest() {
                match response {
                    Ok(protocol) => {
                        state.last_protocol_seq = Some(protocol.seq);
                        state.protocol = Some(protocol);
                        state.media_error = None;
                    }
                    Err(error) => state.media_error = Some(error),
                }
            }
        }

        state.pending_frame_drops = capture_session.stats().dropped_i420_frames;
        state.encoded_frames = encoded_frames;

        while event::poll(Duration::from_millis(1))? {
            let CrosstermEvent::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('q') {
                return Ok(());
            }
        }

        terminal.draw(|frame| render_ui(frame, &mut state, kitty_available))?;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    Ok(())
}

fn local_screen_frame_to_rgba(
    path: LocalScreenPath,
    profile: ScreenCaptureProfile,
    encoder: &mut Option<Vp8VideoEncoder>,
    decoder: &mut Option<Vp8VideoDecoder>,
    seq: u32,
    frame: rchat_screen_capture::I420ScreenFrame,
) -> Result<Option<(DecodedRgbaFrame, u64)>> {
    match path {
        LocalScreenPath::Direct => {
            let rgba = i420_to_rgba(&frame.data, frame.width, frame.height)
                .map_err(|error| anyhow!("local I420 to RGBA failed: {error}"))?;
            Ok(Some((
                DecodedRgbaFrame {
                    session_id: "local-screen-smoke".to_string(),
                    seq,
                    timestamp_us: frame.timestamp_us,
                    width: frame.width,
                    height: frame.height,
                    rgba,
                },
                0,
            )))
        }
        LocalScreenPath::Vp8 => {
            let encoder = encoder
                .as_mut()
                .ok_or_else(|| anyhow!("local VP8 encoder is unavailable"))?;
            let decoder = decoder
                .as_mut()
                .ok_or_else(|| anyhow!("local VP8 decoder is unavailable"))?;
            let force_keyframe = seq == 0 || seq % profile.keyframe_interval_frames() == 0;
            let packets = encoder
                .encode_i420(
                    frame.timestamp_us,
                    frame.width,
                    frame.height,
                    &frame.data,
                    force_keyframe,
                )
                .map_err(|error| anyhow!("local VP8 encode failed: {error}"))?;
            let encoded_count = packets.len() as u64;
            let mut decoded = None;
            for packet in packets {
                let rgba = decoder
                    .decode_rgba(&packet.payload)
                    .map_err(|error| anyhow!("local VP8 decode failed: {error}"))?;
                decoded = Some(DecodedRgbaFrame {
                    session_id: "local-screen-smoke".to_string(),
                    seq,
                    timestamp_us: frame.timestamp_us,
                    width: rgba.width,
                    height: rgba.height,
                    rgba: rgba.rgba,
                });
            }
            Ok(decoded.map(|frame| (frame, encoded_count)))
        }
    }
}

async fn create_unlocked_app_state() -> Result<AppState> {
    let app_dir = runtime::default_app_data_dir()?;
    let app_state = runtime::create_app_state(app_dir)?;

    let mut config_manager = app_state.config_manager.lock().await;
    if config_manager.try_restore_session() {
        drop(config_manager);
        return Ok(app_state);
    }

    if !config_manager.exists() {
        return Err(anyhow!(
            "no RChat vault found; create and unlock the GUI app once before using rchat-tui"
        ));
    }

    let password = rpassword::prompt_password("RChat vault password: ")?;
    config_manager.unlock_with_password(password.trim()).await?;
    drop(config_manager);
    Ok(app_state)
}

async fn send_network_command(network_state: &NetworkState, command: NetworkCommand) -> Result<()> {
    let sender = network_state.sender.lock().await.clone();
    sender
        .send(command)
        .await
        .map_err(|_| anyhow!("network command channel is closed"))
}

fn drain_core_events(
    event_rx: &mut mpsc::Receiver<TuiEvent>,
    state: &mut UiState,
    pending_frames: &mut LatestFrameSlot<BroadcastFrameEvent>,
    decoder: &mut ScreenFrameDecoder,
) {
    while let Ok(event) = event_rx.try_recv() {
        match event {
            TuiEvent::Core(CoreEvent::ConnectedChatIdsUpdated(ids)) => {
                state.connected_chat_ids = ids;
            }
            TuiEvent::Core(CoreEvent::BroadcastStateUpdated(next)) => {
                apply_broadcast_state(state, decoder, next);
            }
            TuiEvent::Core(CoreEvent::BroadcastFrame(frame)) => {
                state.received_frames = state.received_frames.saturating_add(1);
                pending_frames.push(frame);
            }
            TuiEvent::Core(CoreEvent::ScreenBroadcastCaptureError(error)) => {
                state.media_error = Some(error.message);
            }
            TuiEvent::Core(CoreEvent::LocalPeerDiscovered(peer)) => {
                state.last_peer_event = Some(format!("discovered {}", peer.peer_id));
            }
            TuiEvent::Core(CoreEvent::LocalPeerExpired(peer_id)) => {
                state.last_peer_event = Some(format!("expired {peer_id}"));
            }
            _ => {}
        }
    }
}

fn apply_broadcast_state(
    state: &mut UiState,
    decoder: &mut ScreenFrameDecoder,
    next: BroadcastState,
) {
    state.broadcast_state = next.clone();
    state.incoming_session_id = if next.phase == BroadcastPhase::IncomingRinging {
        next.session_id.clone()
    } else {
        None
    };
    state.active_session_id = if next.phase == BroadcastPhase::Active {
        next.session_id.clone()
    } else {
        None
    };
    if next.phase == BroadcastPhase::Idle {
        decoder.clear();
        state.protocol = None;
        state.last_protocol_seq = None;
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("failed to start terminal")?;
        Ok(Self { terminal })
    }

    fn draw<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut Frame<'_>),
    {
        self.terminal.draw(f)?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

struct UiState {
    status: String,
    protocol_type: ProtocolType,
    event_sink: TuiEventSink,
    broadcast_state: BroadcastState,
    incoming_session_id: Option<String>,
    active_session_id: Option<String>,
    connected_chat_ids: Vec<String>,
    last_peer_event: Option<String>,
    received_frames: u64,
    encoded_frames: u64,
    decoded_frames: u64,
    pending_frame_drops: u64,
    decoder_dropped_delta: u64,
    decoder_errors: u64,
    media_error: Option<String>,
    media_size: Size,
    protocol: Option<ProtocolResponse>,
    last_protocol_seq: Option<u32>,
    show_help: bool,
}

impl UiState {
    fn new(protocol_type: ProtocolType, event_sink: TuiEventSink) -> Self {
        Self {
            status: "network running".to_string(),
            protocol_type,
            event_sink,
            broadcast_state: BroadcastState::default(),
            incoming_session_id: None,
            active_session_id: None,
            connected_chat_ids: Vec::new(),
            last_peer_event: None,
            received_frames: 0,
            encoded_frames: 0,
            decoded_frames: 0,
            pending_frame_drops: 0,
            decoder_dropped_delta: 0,
            decoder_errors: 0,
            media_error: None,
            media_size: Size::new(80, 24),
            protocol: None,
            last_protocol_seq: None,
            show_help: true,
        }
    }
}

fn render_ui(frame: &mut Frame<'_>, state: &mut UiState, kitty_available: bool) {
    let root = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(20)])
        .split(frame.area());
    render_status(frame, root[0], state, kitty_available);
    render_media(frame, root[1], state, kitty_available);
}

fn render_status(frame: &mut Frame<'_>, area: Rect, state: &UiState, kitty_available: bool) {
    let mut lines = vec![
        Line::from(Span::styled(
            "RChat TUI",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("Status: {}", state.status)),
        Line::from(format!("Protocol: {:?}", state.protocol_type)),
        Line::from(format!(
            "Kitty media: {}",
            if kitty_available { "yes" } else { "no" }
        )),
        Line::from(format!("Connected chats: {}", state.connected_chat_ids.len())),
        Line::from(format!("Broadcast: {:?}", state.broadcast_state.phase)),
        Line::from(format!(
            "Incoming: {}",
            state.incoming_session_id.as_deref().unwrap_or("-")
        )),
        Line::from(format!(
            "Active: {}",
            state.active_session_id.as_deref().unwrap_or("-")
        )),
        Line::from(""),
        Line::from(format!("Received frames: {}", state.received_frames)),
        Line::from(format!("Encoded frames: {}", state.encoded_frames)),
        Line::from(format!("Decoded frames: {}", state.decoded_frames)),
        Line::from(format!("Pending drops: {}", state.pending_frame_drops)),
        Line::from(format!("Pre-key deltas: {}", state.decoder_dropped_delta)),
        Line::from(format!("Decode errors: {}", state.decoder_errors)),
        Line::from(format!("Event drops: {}", state.event_sink.dropped_events())),
        Line::from(format!(
            "Last seq: {}",
            state
                .last_protocol_seq
                .map(|seq| seq.to_string())
                .unwrap_or_else(|| "-".to_string())
        )),
    ];

    if let Some(peer_event) = &state.last_peer_event {
        lines.push(Line::from(""));
        lines.push(Line::from(peer_event.clone()));
    }
    if let Some(error) = &state.media_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    if state.show_help {
        lines.push(Line::from(""));
        lines.push(Line::from("q quit"));
        lines.push(Line::from("a accept screen"));
        lines.push(Line::from("r reject screen"));
        lines.push(Line::from("e end screen"));
        lines.push(Line::from("? help"));
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::default().title("Control").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_media(frame: &mut Frame<'_>, area: Rect, state: &mut UiState, kitty_available: bool) {
    let block = Block::default().title("Screen").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    state.media_size = Size::new(inner.width, inner.height);

    if !kitty_available {
        let message = Paragraph::new("Kitty image protocol unavailable; run inside Ratty.")
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: true });
        frame.render_widget(message, inner);
        return;
    }

    if let Some(protocol) = &state.protocol {
        let image = Image::new(&protocol.protocol);
        frame.render_widget(image, inner);
    } else {
        let message = Paragraph::new("Waiting for screen-share frames...")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(message, inner);
    }
}
