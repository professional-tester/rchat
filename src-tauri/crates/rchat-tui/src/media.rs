use anyhow::{anyhow, Context, Result};
use image::{
    imageops::{overlay, FilterType},
    DynamicImage, RgbaImage,
};
use ratatui::layout::Size;
use ratatui_image::{picker::Picker, protocol::Protocol, FontSize, Resize};
use rchat_core::{
    events::VideoEncodedRemoteFrameEvent,
    live::{
        broadcast::protocol::{BroadcastChunkType, BroadcastFrameEvent},
        video::{
            codec::{RgbaVideoFrame, Vp8VideoDecoder},
            protocol::VideoChunkType,
        },
    },
};
use std::collections::{HashMap, VecDeque};
use std::sync::{mpsc, Arc};
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

    fn from_remote_video_frame(
        event: &VideoEncodedRemoteFrameEvent,
        frame: RgbaVideoFrame,
    ) -> Self {
        Self {
            session_id: event.call_id.clone(),
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

#[derive(Default)]
pub struct RemoteVideoFrameDecoder {
    call_id: Option<String>,
    decoder: Option<Vp8VideoDecoder>,
    has_keyframe: bool,
    dropped_delta_before_keyframe: u64,
    decode_errors: u64,
}

impl RemoteVideoFrameDecoder {
    pub fn decode_event(
        &mut self,
        event: &VideoEncodedRemoteFrameEvent,
    ) -> Option<DecodedRgbaFrame> {
        if self.call_id.as_deref() != Some(event.call_id.as_str()) {
            self.reset_for_call(event.call_id.clone());
        }

        if event.chunk_type == VideoChunkType::Delta && !self.has_keyframe {
            self.dropped_delta_before_keyframe =
                self.dropped_delta_before_keyframe.saturating_add(1);
            return None;
        }

        if event.chunk_type == VideoChunkType::Key {
            self.has_keyframe = true;
        }

        let Some(decoder) = self.decoder.as_mut() else {
            self.decode_errors = self.decode_errors.saturating_add(1);
            return None;
        };

        match decoder.decode_rgba(&event.payload) {
            Ok(frame) => Some(DecodedRgbaFrame::from_remote_video_frame(event, frame)),
            Err(_) => {
                self.decode_errors = self.decode_errors.saturating_add(1);
                None
            }
        }
    }

    pub fn clear(&mut self) {
        self.call_id = None;
        self.decoder = None;
        self.has_keyframe = false;
    }

    pub fn dropped_delta_before_keyframe(&self) -> u64 {
        self.dropped_delta_before_keyframe
    }

    pub fn decode_errors(&self) -> u64 {
        self.decode_errors
    }

    fn reset_for_call(&mut self, call_id: String) {
        self.call_id = Some(call_id);
        self.decoder = Vp8VideoDecoder::new().ok();
        self.has_keyframe = false;
    }
}

pub fn rgba_frame_to_dynamic_image(frame: &DecodedRgbaFrame) -> Result<DynamicImage> {
    let image = RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| anyhow!("invalid RGBA frame dimensions"))?;
    Ok(DynamicImage::ImageRgba8(image))
}

pub fn decode_inline_media_preview(data: &[u8]) -> Result<DynamicImage> {
    image::load_from_memory(data).context("failed to decode inline media image")
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InlineMediaKey {
    pub message_id: String,
    pub file_hash: String,
    pub size: Size,
}

impl InlineMediaKey {
    pub fn new(message_id: impl Into<String>, file_hash: impl Into<String>, size: Size) -> Self {
        Self {
            message_id: message_id.into(),
            file_hash: file_hash.into(),
            size,
        }
    }
}

pub enum InlineMediaState {
    Loading,
    Error(String),
    Ready(Protocol),
}

pub struct InlineMediaCache {
    capacity: usize,
    entries: HashMap<InlineMediaKey, InlineMediaState>,
    order: VecDeque<InlineMediaKey>,
}

impl InlineMediaCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn get(&self, key: &InlineMediaKey) -> Option<&InlineMediaState> {
        self.entries.get(key)
    }

    pub fn insert_loading(&mut self, key: InlineMediaKey) {
        self.insert_state(key, InlineMediaState::Loading);
    }

    pub fn insert_error(&mut self, key: InlineMediaKey, error: impl Into<String>) {
        self.insert_state(key, InlineMediaState::Error(error.into()));
    }

    pub fn insert_ready(&mut self, key: InlineMediaKey, protocol: Protocol) {
        self.insert_state(key, InlineMediaState::Ready(protocol));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    fn insert_state(&mut self, key: InlineMediaKey, state: InlineMediaState) {
        if self.capacity == 0 {
            self.entries.clear();
            self.order.clear();
            return;
        }

        self.order.retain(|existing| existing != &key);
        self.order.push_back(key.clone());
        self.entries.insert(key, state);

        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MediaViewerKey {
    pub file_hash: String,
    pub size: Size,
    pub zoom_percent: u16,
    pub pan_x: i32,
    pub pan_y: i32,
}

impl MediaViewerKey {
    pub fn new(
        file_hash: impl Into<String>,
        size: Size,
        zoom_percent: u16,
        pan_x: i32,
        pan_y: i32,
    ) -> Self {
        Self {
            file_hash: file_hash.into(),
            size,
            zoom_percent,
            pan_x,
            pan_y,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProtocolRequestId {
    Screen(u32),
    RemoteVideo { call_id: String, seq: u32 },
    Inline(InlineMediaKey),
    Viewer(MediaViewerKey),
}

pub struct ProtocolRequest {
    pub id: ProtocolRequestId,
    pub image: Arc<DynamicImage>,
    pub size: Size,
}

impl ProtocolRequest {
    pub fn screen(frame: DecodedRgbaFrame, size: Size) -> Result<Self> {
        let seq = frame.seq;
        Ok(Self {
            id: ProtocolRequestId::Screen(seq),
            image: Arc::new(rgba_frame_to_dynamic_image(&frame)?),
            size,
        })
    }

    pub fn remote_video(frame: DecodedRgbaFrame, size: Size) -> Result<Self> {
        let call_id = frame.session_id.clone();
        let seq = frame.seq;
        Ok(Self {
            id: ProtocolRequestId::RemoteVideo { call_id, seq },
            image: Arc::new(rgba_frame_to_dynamic_image(&frame)?),
            size,
        })
    }

    pub fn inline(key: InlineMediaKey, image: DynamicImage, size: Size) -> Self {
        Self {
            id: ProtocolRequestId::Inline(key),
            image: Arc::new(image),
            size,
        }
    }

    pub fn viewer(key: MediaViewerKey, image: Arc<DynamicImage>, size: Size) -> Self {
        Self {
            id: ProtocolRequestId::Viewer(key),
            image,
            size,
        }
    }
}

pub struct ProtocolResponse {
    pub id: ProtocolRequestId,
    pub protocol: Protocol,
}

pub struct ProtocolError {
    pub id: ProtocolRequestId,
    pub message: String,
}

pub struct ProtocolWorker {
    tx: mpsc::Sender<ProtocolRequest>,
    rx: mpsc::Receiver<Result<ProtocolResponse, ProtocolError>>,
}

impl ProtocolWorker {
    pub fn spawn(picker: Picker) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<ProtocolRequest>();
        let (response_tx, response_rx) = mpsc::channel::<Result<ProtocolResponse, ProtocolError>>();

        thread::spawn(move || {
            let mut viewer_resize_cache = ViewerResizeCache::default();
            while let Ok(request) = request_rx.recv() {
                let mut requests = vec![request];
                while let Ok(newer) = request_rx.try_recv() {
                    requests.push(newer);
                }

                for request in select_protocol_requests_for_processing(requests) {
                    if !send_protocol_response(
                        &response_tx,
                        &picker,
                        &mut viewer_resize_cache,
                        request,
                    ) {
                        return;
                    }
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

    pub fn try_recv_latest(&self) -> Option<Result<ProtocolResponse, ProtocolError>> {
        let mut latest = None;
        while let Ok(response) = self.rx.try_recv() {
            latest = Some(response);
        }
        latest
    }

    pub fn try_recv_all(&self) -> Vec<Result<ProtocolResponse, ProtocolError>> {
        let mut responses = Vec::new();
        while let Ok(response) = self.rx.try_recv() {
            responses.push(response);
        }
        responses
    }
}

fn select_protocol_requests_for_processing(requests: Vec<ProtocolRequest>) -> Vec<ProtocolRequest> {
    let mut inline_requests = Vec::new();
    let mut latest_viewer = None;
    let mut latest_screen = None;
    let mut latest_remote_video = None;

    for request in requests {
        match &request.id {
            ProtocolRequestId::Screen(_) => latest_screen = Some(request),
            ProtocolRequestId::RemoteVideo { .. } => latest_remote_video = Some(request),
            ProtocolRequestId::Inline(_) => inline_requests.push(request),
            ProtocolRequestId::Viewer(_) => latest_viewer = Some(request),
        }
    }

    if let Some(request) = latest_viewer {
        inline_requests.push(request);
    }
    if let Some(request) = latest_screen {
        inline_requests.push(request);
    }
    if let Some(request) = latest_remote_video {
        inline_requests.push(request);
    }
    inline_requests
}

fn send_protocol_response(
    response_tx: &mpsc::Sender<Result<ProtocolResponse, ProtocolError>>,
    picker: &Picker,
    viewer_resize_cache: &mut ViewerResizeCache,
    request: ProtocolRequest,
) -> bool {
    let id = request.id.clone();
    let error_id = id.clone();
    let result = build_protocol_for_request(picker, request, viewer_resize_cache)
        .map(|protocol| ProtocolResponse { id, protocol })
        .map_err(|error| ProtocolError {
            id: error_id,
            message: error.to_string(),
        });

    response_tx.send(result).is_ok()
}

fn build_protocol_for_request(
    picker: &Picker,
    request: ProtocolRequest,
    viewer_resize_cache: &mut ViewerResizeCache,
) -> Result<Protocol> {
    let image = match &request.id {
        ProtocolRequestId::Viewer(key) => prepare_media_viewer_image(
            request.image.as_ref(),
            key,
            picker.font_size(),
            viewer_resize_cache,
        ),
        ProtocolRequestId::Screen(_)
        | ProtocolRequestId::RemoteVideo { .. }
        | ProtocolRequestId::Inline(_) => shared_image_into_owned(request.image),
    };
    build_protocol(picker, image, request.size)
}

fn shared_image_into_owned(image: Arc<DynamicImage>) -> DynamicImage {
    Arc::try_unwrap(image).unwrap_or_else(|image| image.as_ref().clone())
}

fn build_protocol(picker: &Picker, image: DynamicImage, size: Size) -> Result<Protocol> {
    picker
        .new_protocol(image, size, Resize::Fit(None))
        .map_err(|error| anyhow!(error.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewerResizeCacheKey {
    file_hash: String,
    size: Size,
    zoom_percent: u16,
    font_width: u16,
    font_height: u16,
}

impl ViewerResizeCacheKey {
    fn new(key: &MediaViewerKey, font_size: FontSize) -> Self {
        Self {
            file_hash: key.file_hash.clone(),
            size: key.size,
            zoom_percent: key.zoom_percent,
            font_width: font_size.width,
            font_height: font_size.height,
        }
    }
}

#[derive(Debug, Default)]
struct ViewerResizeCache {
    key: Option<ViewerResizeCacheKey>,
    resized: Option<RgbaImage>,
    resize_count: u64,
}

impl ViewerResizeCache {
    fn resized_layer(
        &mut self,
        key: ViewerResizeCacheKey,
        image: &DynamicImage,
        scaled_width: u32,
        scaled_height: u32,
    ) -> &RgbaImage {
        if self.key.as_ref() != Some(&key) {
            self.resized = Some(
                image
                    .resize(scaled_width, scaled_height, FilterType::Triangle)
                    .to_rgba8(),
            );
            self.key = Some(key);
            self.resize_count = self.resize_count.saturating_add(1);
        }

        self.resized
            .as_ref()
            .expect("viewer resize cache is populated")
    }

    #[cfg(test)]
    fn resize_count(&self) -> u64 {
        self.resize_count
    }
}

fn prepare_media_viewer_image(
    image: &DynamicImage,
    key: &MediaViewerKey,
    font_size: FontSize,
    viewer_resize_cache: &mut ViewerResizeCache,
) -> DynamicImage {
    let viewport_width =
        u32::from(key.size.width.max(1)).saturating_mul(u32::from(font_size.width.max(1)));
    let viewport_height =
        u32::from(key.size.height.max(1)).saturating_mul(u32::from(font_size.height.max(1)));
    let image_width = image.width().max(1);
    let image_height = image.height().max(1);

    let fit_scale = (viewport_width as f32 / image_width as f32)
        .min(viewport_height as f32 / image_height as f32)
        .max(0.01);
    let zoom_scale = f32::from(key.zoom_percent.max(25)) / 100.0;
    let scaled_width = ((image_width as f32 * fit_scale * zoom_scale).round() as u32).max(1);
    let scaled_height = ((image_height as f32 * fit_scale * zoom_scale).round() as u32).max(1);
    let resized = viewer_resize_cache.resized_layer(
        ViewerResizeCacheKey::new(key, font_size),
        image,
        scaled_width,
        scaled_height,
    );
    let scaled_width = resized.width();
    let scaled_height = resized.height();

    let mut canvas = RgbaImage::new(viewport_width, viewport_height);
    let base_x = (i64::from(viewport_width) - i64::from(scaled_width)) / 2;
    let base_y = (i64::from(viewport_height) - i64::from(scaled_height)) / 2;
    let image_x = base_x.saturating_sub(i64::from(key.pan_x));
    let image_y = base_y.saturating_sub(i64::from(key.pan_y));

    let src_x = if image_x < 0 {
        (-image_x).min(i64::from(scaled_width)) as u32
    } else {
        0
    };
    let src_y = if image_y < 0 {
        (-image_y).min(i64::from(scaled_height)) as u32
    } else {
        0
    };
    let dest_x = image_x.max(0).min(i64::from(viewport_width)) as u32;
    let dest_y = image_y.max(0).min(i64::from(viewport_height)) as u32;
    let copy_width = scaled_width
        .saturating_sub(src_x)
        .min(viewport_width.saturating_sub(dest_x));
    let copy_height = scaled_height
        .saturating_sub(src_y)
        .min(viewport_height.saturating_sub(dest_y));

    if copy_width > 0 && copy_height > 0 {
        let cropped =
            image::imageops::crop_imm(resized, src_x, src_y, copy_width, copy_height).to_image();
        overlay(&mut canvas, &cropped, i64::from(dest_x), i64::from(dest_y));
    }

    DynamicImage::ImageRgba8(canvas)
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
        let mut encoder = Vp8VideoEncoder::new_with_dimensions(VideoProfile::P360, width, height)
            .expect("encoder starts");
        let packets = encoder
            .encode_i420(123, width, height, &synthetic_i420(width, height), true)
            .expect("frame encodes");
        packets.into_iter().next().expect("packet").payload
    }

    fn frame_event(
        session_id: &str,
        seq: u32,
        chunk_type: BroadcastChunkType,
    ) -> BroadcastFrameEvent {
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

    fn remote_video_event(
        call_id: &str,
        seq: u32,
        chunk_type: VideoChunkType,
    ) -> VideoEncodedRemoteFrameEvent {
        VideoEncodedRemoteFrameEvent {
            call_id: call_id.to_string(),
            peer_id: "peer-1".to_string(),
            seq,
            timestamp: 123,
            mime: "video/webm;codecs=vp8".to_string(),
            codec: "vp8".to_string(),
            chunk_type,
            profile: "360p".to_string(),
            width: 16,
            height: 16,
            payload: if chunk_type == VideoChunkType::Key {
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
    fn remote_video_decoder_drops_delta_before_keyframe() {
        let mut decoder = RemoteVideoFrameDecoder::default();
        let event = remote_video_event("call-1", 1, VideoChunkType::Delta);

        assert!(decoder.decode_event(&event).is_none());
        assert_eq!(decoder.dropped_delta_before_keyframe(), 1);
        assert_eq!(decoder.decode_errors(), 0);
    }

    #[test]
    fn remote_video_decoder_decodes_keyframes_by_call_id() {
        let mut decoder = RemoteVideoFrameDecoder::default();

        let frame = decoder
            .decode_event(&remote_video_event("call-1", 7, VideoChunkType::Key))
            .expect("remote keyframe decodes");

        assert_eq!(frame.session_id, "call-1");
        assert_eq!(frame.seq, 7);
        assert_eq!(frame.width, 16);
        assert_eq!(frame.height, 16);
        assert_eq!(frame.rgba.len(), 16 * 16 * 4);
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

    #[test]
    fn inline_media_cache_key_includes_hash_message_and_size() {
        let key_a = InlineMediaKey::new("m1", "hash-1", Size::new(20, 8));
        let key_b = InlineMediaKey::new("m1", "hash-1", Size::new(20, 9));
        let key_c = InlineMediaKey::new("m2", "hash-1", Size::new(20, 8));

        assert_ne!(key_a, key_b);
        assert_ne!(key_a, key_c);
        assert_eq!(key_a.file_hash, "hash-1");
        assert_eq!(key_a.message_id, "m1");
        assert_eq!(key_a.size, Size::new(20, 8));
    }

    #[test]
    fn inline_media_cache_evicts_oldest_entries() {
        let mut cache = InlineMediaCache::new(2);
        let key_a = InlineMediaKey::new("m1", "hash-1", Size::new(20, 8));
        let key_b = InlineMediaKey::new("m2", "hash-2", Size::new(20, 8));
        let key_c = InlineMediaKey::new("m3", "hash-3", Size::new(20, 8));

        cache.insert_loading(key_a.clone());
        cache.insert_loading(key_b.clone());
        cache.insert_loading(key_c.clone());

        assert!(cache.get(&key_a).is_none());
        assert!(cache.get(&key_b).is_some());
        assert!(cache.get(&key_c).is_some());
    }

    #[test]
    fn invalid_inline_image_bytes_return_error() {
        let err = decode_inline_media_preview(&[1, 2, 3]).expect_err("invalid image bytes fail");
        assert!(err.to_string().contains("decode"));
    }

    #[test]
    fn protocol_request_accepts_dynamic_images_for_inline_media() {
        let image = DynamicImage::new_rgba8(2, 2);
        let key = InlineMediaKey::new("m1", "hash-1", Size::new(20, 8));
        let request = ProtocolRequest::inline(key.clone(), image, Size::new(20, 8));

        assert_eq!(request.id, ProtocolRequestId::Inline(key));
        assert_eq!(request.size, Size::new(20, 8));
    }

    #[test]
    fn media_viewer_key_includes_hash_size_zoom_and_pan() {
        let key_a = MediaViewerKey::new("hash-1", Size::new(80, 24), 100, 0, 0);
        let key_b = MediaViewerKey::new("hash-1", Size::new(80, 24), 125, 0, 0);
        let key_c = MediaViewerKey::new("hash-1", Size::new(80, 24), 100, 4, 0);
        let key_d = MediaViewerKey::new("hash-1", Size::new(80, 20), 100, 0, 0);

        assert_ne!(key_a, key_b);
        assert_ne!(key_a, key_c);
        assert_ne!(key_a, key_d);
        assert_eq!(key_a.file_hash, "hash-1");
    }

    #[test]
    fn protocol_request_accepts_dynamic_images_for_media_viewer() {
        let image = Arc::new(DynamicImage::new_rgba8(2, 2));
        let key = MediaViewerKey::new("hash-1", Size::new(80, 24), 100, 0, 0);
        let request = ProtocolRequest::viewer(key.clone(), Arc::clone(&image), Size::new(80, 24));

        assert_eq!(request.id, ProtocolRequestId::Viewer(key));
        assert_eq!(request.size, Size::new(80, 24));
        assert!(Arc::ptr_eq(&request.image, &image));
        assert_eq!(Arc::strong_count(&image), 2);
    }

    #[test]
    fn media_viewer_fit_image_is_centered_on_viewport_canvas() {
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 2, image::Rgba([240, 10, 20, 255])));
        let key = MediaViewerKey::new("hash-1", Size::new(1, 1), 100, 0, 0);

        let mut cache = ViewerResizeCache::default();
        let prepared =
            prepare_media_viewer_image(&image, &key, FontSize::new(10, 20), &mut cache).to_rgba8();

        assert_eq!(prepared.width(), 10);
        assert_eq!(prepared.height(), 20);
        assert_eq!(prepared.get_pixel(0, 0)[3], 0);
        assert_eq!(prepared.get_pixel(0, 7), &image::Rgba([240, 10, 20, 255]));
        assert_eq!(prepared.get_pixel(0, 11), &image::Rgba([240, 10, 20, 255]));
        assert_eq!(prepared.get_pixel(0, 19)[3], 0);
    }

    #[test]
    fn media_viewer_zoom_starts_from_center_and_pan_moves_crop() {
        let mut source = RgbaImage::new(16, 16);
        for y in 0..16 {
            for x in 0..16 {
                let value = (x * 16) as u8;
                source.put_pixel(x, y, image::Rgba([value, value, value, 255]));
            }
        }

        let centered_key = MediaViewerKey::new("hash-1", Size::new(1, 1), 200, 0, 0);
        let moved_key = MediaViewerKey::new("hash-1", Size::new(1, 1), 200, 4, 0);

        let centered = {
            let mut cache = ViewerResizeCache::default();
            prepare_media_viewer_image(
                &DynamicImage::ImageRgba8(source.clone()),
                &centered_key,
                FontSize::new(10, 20),
                &mut cache,
            )
        }
        .to_rgba8();
        let mut moved_cache = ViewerResizeCache::default();
        let moved = prepare_media_viewer_image(
            &DynamicImage::ImageRgba8(source),
            &moved_key,
            FontSize::new(10, 20),
            &mut moved_cache,
        )
        .to_rgba8();

        assert!(centered.get_pixel(0, 0)[0] >= 48);
        assert!(moved.get_pixel(0, 0)[0] > centered.get_pixel(0, 0)[0]);
    }

    #[test]
    fn media_viewer_resize_cache_reuses_zoom_layer_for_pan_only() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            16,
            16,
            image::Rgba([10, 200, 50, 255]),
        ));
        let mut cache = ViewerResizeCache::default();
        let centered_key = MediaViewerKey::new("hash-1", Size::new(1, 1), 200, 0, 0);
        let panned_key = MediaViewerKey::new("hash-1", Size::new(1, 1), 200, 0, -6);
        let zoomed_key = MediaViewerKey::new("hash-1", Size::new(1, 1), 225, 0, -6);

        let _ =
            prepare_media_viewer_image(&image, &centered_key, FontSize::new(10, 20), &mut cache);
        assert_eq!(cache.resize_count(), 1);

        let _ = prepare_media_viewer_image(&image, &panned_key, FontSize::new(10, 20), &mut cache);
        assert_eq!(cache.resize_count(), 1);

        let _ = prepare_media_viewer_image(&image, &zoomed_key, FontSize::new(10, 20), &mut cache);
        assert_eq!(cache.resize_count(), 2);
    }

    #[test]
    fn media_viewer_pan_beyond_top_edge_moves_image_on_canvas() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            16,
            16,
            image::Rgba([10, 200, 50, 255]),
        ));
        let key = MediaViewerKey::new("hash-1", Size::new(1, 1), 200, 0, -6);

        let mut cache = ViewerResizeCache::default();
        let prepared =
            prepare_media_viewer_image(&image, &key, FontSize::new(10, 20), &mut cache).to_rgba8();

        assert_eq!(prepared.width(), 10);
        assert_eq!(prepared.height(), 20);
        assert_eq!(prepared.get_pixel(0, 0)[3], 0);
        assert_eq!(prepared.get_pixel(0, 6), &image::Rgba([10, 200, 50, 255]));
    }

    #[test]
    fn protocol_request_accepts_remote_video_frames() {
        let frame = DecodedRgbaFrame {
            session_id: "call-1".to_string(),
            seq: 4,
            timestamp_us: 123,
            width: 2,
            height: 2,
            rgba: vec![0; 2 * 2 * 4],
        };

        let request = ProtocolRequest::remote_video(frame, Size::new(20, 8)).unwrap();

        assert_eq!(
            request.id,
            ProtocolRequestId::RemoteVideo {
                call_id: "call-1".to_string(),
                seq: 4
            }
        );
        assert_eq!(request.size, Size::new(20, 8));
    }

    #[test]
    fn protocol_worker_processes_all_inline_and_viewer_requests_but_only_latest_screen() {
        let inline_a = InlineMediaKey::new("m1", "hash-1", Size::new(20, 8));
        let inline_b = InlineMediaKey::new("m2", "hash-2", Size::new(20, 8));
        let viewer = MediaViewerKey::new("hash-3", Size::new(80, 24), 100, 0, 0);
        let requests = vec![
            ProtocolRequest {
                id: ProtocolRequestId::Screen(1),
                image: Arc::new(DynamicImage::new_rgba8(2, 2)),
                size: Size::new(20, 8),
            },
            ProtocolRequest::inline(
                inline_a.clone(),
                DynamicImage::new_rgba8(2, 2),
                Size::new(20, 8),
            ),
            ProtocolRequest {
                id: ProtocolRequestId::Screen(2),
                image: Arc::new(DynamicImage::new_rgba8(2, 2)),
                size: Size::new(20, 8),
            },
            ProtocolRequest::inline(
                inline_b.clone(),
                DynamicImage::new_rgba8(2, 2),
                Size::new(20, 8),
            ),
            ProtocolRequest::viewer(
                viewer.clone(),
                Arc::new(DynamicImage::new_rgba8(2, 2)),
                Size::new(80, 24),
            ),
        ];

        let ids = select_protocol_requests_for_processing(requests)
            .into_iter()
            .map(|request| request.id)
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            vec![
                ProtocolRequestId::Inline(inline_a),
                ProtocolRequestId::Inline(inline_b),
                ProtocolRequestId::Viewer(viewer),
                ProtocolRequestId::Screen(2),
            ]
        );
    }
}
