use anyhow::{anyhow, Result};
use image::{DynamicImage, RgbaImage};
use ratatui::layout::Size;
use ratatui_image::{picker::Picker, protocol::Protocol, Resize};
use rchat_core::live::{
    broadcast::protocol::{BroadcastChunkType, BroadcastFrameEvent},
    video::codec::{RgbaVideoFrame, Vp8VideoDecoder},
};
use std::sync::mpsc;
use std::thread;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRgbaFrame {
    pub session_id: String,
    pub seq: u32,
    pub timestamp_us: i64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl DecodedRgbaFrame {
    fn from_video_frame(event: &BroadcastFrameEvent, frame: RgbaVideoFrame) -> Self {
        Self {
            session_id: event.session_id.clone(),
            seq: event.seq,
            timestamp_us: event.timestamp,
            width: frame.width,
            height: frame.height,
            rgba: frame.rgba,
        }
    }
}

#[derive(Debug)]
pub struct LatestFrameSlot<T> {
    frame: Option<T>,
    dropped_frames: u64,
}

impl<T> Default for LatestFrameSlot<T> {
    fn default() -> Self {
        Self {
            frame: None,
            dropped_frames: 0,
        }
    }
}

impl<T> LatestFrameSlot<T> {
    pub fn push(&mut self, frame: T) {
        if self.frame.replace(frame).is_some() {
            self.dropped_frames = self.dropped_frames.saturating_add(1);
        }
    }

    pub fn take(&mut self) -> Option<T> {
        self.frame.take()
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }
}

#[derive(Default)]
pub struct ScreenFrameDecoder {
    session_id: Option<String>,
    decoder: Option<Vp8VideoDecoder>,
    has_keyframe: bool,
    dropped_delta_before_keyframe: u64,
    decode_errors: u64,
    session_resets: u64,
}

impl ScreenFrameDecoder {
    pub fn decode_event(&mut self, event: &BroadcastFrameEvent) -> Option<DecodedRgbaFrame> {
        if self.session_id.as_deref() != Some(event.session_id.as_str()) {
            self.reset_for_session(event.session_id.clone());
        }

        if event.chunk_type == BroadcastChunkType::Delta && !self.has_keyframe {
            self.dropped_delta_before_keyframe =
                self.dropped_delta_before_keyframe.saturating_add(1);
            return None;
        }

        if event.chunk_type == BroadcastChunkType::Key {
            self.has_keyframe = true;
        }

        let Some(decoder) = self.decoder.as_mut() else {
            self.decode_errors = self.decode_errors.saturating_add(1);
            return None;
        };

        match decoder.decode_rgba(&event.payload) {
            Ok(frame) => Some(DecodedRgbaFrame::from_video_frame(event, frame)),
            Err(_) => {
                self.decode_errors = self.decode_errors.saturating_add(1);
                None
            }
        }
    }

    pub fn clear(&mut self) {
        self.session_id = None;
        self.decoder = None;
        self.has_keyframe = false;
    }

    pub fn current_session(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn dropped_delta_before_keyframe(&self) -> u64 {
        self.dropped_delta_before_keyframe
    }

    pub fn decode_errors(&self) -> u64 {
        self.decode_errors
    }

    pub fn session_resets(&self) -> u64 {
        self.session_resets
    }

    fn reset_for_session(&mut self, session_id: String) {
        if self.session_id.is_some() {
            self.session_resets = self.session_resets.saturating_add(1);
        }
        self.session_id = Some(session_id);
        self.decoder = Vp8VideoDecoder::new().ok();
        self.has_keyframe = false;
    }
}

pub fn rgba_frame_to_dynamic_image(frame: &DecodedRgbaFrame) -> Result<DynamicImage> {
    let image = RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| anyhow!("invalid RGBA frame dimensions"))?;
    Ok(DynamicImage::ImageRgba8(image))
}

#[derive(Debug)]
pub struct ProtocolRequest {
    pub frame: DecodedRgbaFrame,
    pub size: Size,
}

pub struct ProtocolResponse {
    pub seq: u32,
    pub protocol: Protocol,
}

pub struct ProtocolWorker {
    tx: mpsc::Sender<ProtocolRequest>,
    rx: mpsc::Receiver<Result<ProtocolResponse, String>>,
}

impl ProtocolWorker {
    pub fn spawn(picker: Picker) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<ProtocolRequest>();
        let (response_tx, response_rx) = mpsc::channel::<Result<ProtocolResponse, String>>();

        thread::spawn(move || {
            while let Ok(mut request) = request_rx.recv() {
                while let Ok(newer) = request_rx.try_recv() {
                    request = newer;
                }

                let seq = request.frame.seq;
                let result = build_protocol(&picker, request.frame, request.size)
                    .map(|protocol| ProtocolResponse { seq, protocol })
                    .map_err(|error| error.to_string());

                if response_tx.send(result).is_err() {
                    break;
                }
            }
        });

        Self {
            tx: request_tx,
            rx: response_rx,
        }
    }

    pub fn request(&self, request: ProtocolRequest) {
        let _ = self.tx.send(request);
    }

    pub fn try_recv_latest(&self) -> Option<Result<ProtocolResponse, String>> {
        let mut latest = None;
        while let Ok(response) = self.rx.try_recv() {
            latest = Some(response);
        }
        latest
    }
}

fn build_protocol(picker: &Picker, frame: DecodedRgbaFrame, size: Size) -> Result<Protocol> {
    let image = rgba_frame_to_dynamic_image(&frame)?;
    picker
        .new_protocol(image, size, Resize::Fit(None))
        .map_err(|error| anyhow!(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rchat_core::live::video::codec::{VideoProfile, Vp8VideoEncoder};

    fn synthetic_i420(width: u32, height: u32) -> Vec<u8> {
        let y_len = (width * height) as usize;
        let uv_len = y_len / 4;
        let mut data = vec![96_u8; y_len];
        data.extend(std::iter::repeat(128_u8).take(uv_len));
        data.extend(std::iter::repeat(128_u8).take(uv_len));
        data
    }

    fn encoded_key_payload(width: u32, height: u32) -> Vec<u8> {
        let mut encoder =
            Vp8VideoEncoder::new_with_dimensions(VideoProfile::P360, width, height)
                .expect("encoder starts");
        let packets = encoder
            .encode_i420(123, width, height, &synthetic_i420(width, height), true)
            .expect("frame encodes");
        packets.into_iter().next().expect("packet").payload
    }

    fn frame_event(session_id: &str, seq: u32, chunk_type: BroadcastChunkType) -> BroadcastFrameEvent {
        BroadcastFrameEvent {
            session_id: session_id.to_string(),
            peer_id: "peer-1".to_string(),
            seq,
            timestamp: 123,
            mime: "video/vp8".to_string(),
            codec: "vp8".to_string(),
            profile: "720p15".to_string(),
            width: 16,
            height: 16,
            chunk_type,
            payload: if chunk_type == BroadcastChunkType::Key {
                encoded_key_payload(16, 16)
            } else {
                vec![1, 2, 3]
            },
        }
    }

    #[test]
    fn latest_frame_slot_keeps_newest_and_counts_drops() {
        let mut slot = LatestFrameSlot::default();

        slot.push(1);
        slot.push(2);

        assert_eq!(slot.dropped_frames(), 1);
        assert_eq!(slot.take(), Some(2));
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn screen_decoder_drops_delta_before_keyframe() {
        let mut decoder = ScreenFrameDecoder::default();
        let event = frame_event("session-1", 1, BroadcastChunkType::Delta);

        assert!(decoder.decode_event(&event).is_none());
        assert_eq!(decoder.dropped_delta_before_keyframe(), 1);
        assert_eq!(decoder.decode_errors(), 0);
    }

    #[test]
    fn screen_decoder_resets_on_session_change() {
        let mut decoder = ScreenFrameDecoder::default();

        let first = decoder
            .decode_event(&frame_event("session-1", 0, BroadcastChunkType::Key))
            .expect("first keyframe decodes");
        let second = decoder
            .decode_event(&frame_event("session-2", 0, BroadcastChunkType::Key))
            .expect("second keyframe decodes");

        assert_eq!(first.session_id, "session-1");
        assert_eq!(second.session_id, "session-2");
        assert_eq!(decoder.current_session(), Some("session-2"));
        assert_eq!(decoder.session_resets(), 1);
    }

    #[test]
    fn rgba_frame_to_dynamic_image_rejects_invalid_length() {
        let frame = DecodedRgbaFrame {
            session_id: "session-1".to_string(),
            seq: 0,
            timestamp_us: 0,
            width: 2,
            height: 2,
            rgba: vec![0; 3],
        };

        assert!(rgba_frame_to_dynamic_image(&frame).is_err());
    }
}
