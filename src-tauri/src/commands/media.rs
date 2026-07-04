use base64::{engine::general_purpose::STANDARD, Engine as _};
use tauri::State;

use crate::{chat::media, settings::stickers, AppState, NetworkState};

pub use crate::settings::stickers::{AddStickerResult, StickerBatchImportResult};

pub type SentMediaResult = media::SentMediaResult;

#[tauri::command]
pub async fn send_image_message(
    peer_id: String,
    file_path: String,
    app_state: State<'_, AppState>,
    net_state: State<'_, NetworkState>,
) -> Result<SentMediaResult, String> {
    media::send_file_from_path(
        app_state.inner(),
        net_state.inner(),
        &peer_id,
        media::MediaKind::Image,
        &file_path,
    )
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn send_document_message(
    peer_id: String,
    file_path: String,
    app_state: State<'_, AppState>,
    net_state: State<'_, NetworkState>,
) -> Result<SentMediaResult, String> {
    media::send_file_from_path(
        app_state.inner(),
        net_state.inner(),
        &peer_id,
        media::MediaKind::Document,
        &file_path,
    )
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn send_video_message(
    peer_id: String,
    file_path: String,
    app_state: State<'_, AppState>,
    net_state: State<'_, NetworkState>,
) -> Result<SentMediaResult, String> {
    media::send_file_from_path(
        app_state.inner(),
        net_state.inner(),
        &peer_id,
        media::MediaKind::Video,
        &file_path,
    )
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn send_audio_message(
    peer_id: String,
    file_path: String,
    app_state: State<'_, AppState>,
    net_state: State<'_, NetworkState>,
) -> Result<SentMediaResult, String> {
    media::send_file_from_path(
        app_state.inner(),
        net_state.inner(),
        &peer_id,
        media::MediaKind::Audio,
        &file_path,
    )
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn send_sticker_message(
    peer_id: String,
    file_hash: String,
    app_state: State<'_, AppState>,
    net_state: State<'_, NetworkState>,
) -> Result<SentMediaResult, String> {
    media::send_sticker(app_state.inner(), net_state.inner(), &peer_id, &file_hash)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_image_data(
    file_hash: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let loaded = media::load_attachment_bytes(state.inner(), &file_hash)
        .map_err(|error| error.to_string())?;
    let mime_type = if loaded.mime_type.starts_with("image/") {
        loaded.mime_type
    } else {
        media::detect_image_mime_from_bytes(&loaded.bytes)
            .unwrap_or("image/png")
            .to_string()
    };
    update_file_mime_type(state.inner(), &file_hash, &mime_type);
    Ok(data_url(&mime_type, &loaded.bytes))
}

#[tauri::command]
pub async fn get_image_from_path(file_path: String) -> Result<String, String> {
    let data =
        std::fs::read(&file_path).map_err(|e| format!("Failed to read image file: {}", e))?;

    let mime_type = if file_path.ends_with(".png") {
        "image/png"
    } else if file_path.ends_with(".jpg") || file_path.ends_with(".jpeg") {
        "image/jpeg"
    } else if file_path.ends_with(".gif") {
        "image/gif"
    } else if file_path.ends_with(".webp") {
        "image/webp"
    } else {
        "image/png"
    };

    Ok(data_url(mime_type, &data))
}

#[tauri::command]
pub async fn get_video_data(
    file_hash: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let loaded = media::load_attachment_bytes(state.inner(), &file_hash)
        .map_err(|error| error.to_string())?;
    let mime_type = if loaded.mime_type.starts_with("video/") {
        loaded.mime_type
    } else {
        "video/mp4".to_string()
    };
    Ok(data_url(&mime_type, &loaded.bytes))
}

#[tauri::command]
pub async fn get_audio_data(
    file_hash: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let loaded = media::load_attachment_bytes(state.inner(), &file_hash)
        .map_err(|error| error.to_string())?;
    let mime_type = if loaded.mime_type.starts_with("audio/") {
        loaded.mime_type
    } else {
        media::detect_audio_mime_from_bytes(&loaded.bytes)
            .unwrap_or("audio/webm")
            .to_string()
    };
    update_file_mime_type(state.inner(), &file_hash, &mime_type);
    Ok(data_url(&mime_type, &loaded.bytes))
}

#[tauri::command]
pub async fn save_image_to_file(
    file_hash: String,
    target_path: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    media::save_attachment_to_path(state.inner(), &file_hash, &target_path)
        .map_err(|error| format!("Failed to save image: {error}"))
}

#[tauri::command]
pub async fn save_document_to_file(
    file_hash: String,
    target_path: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    media::save_attachment_to_path(state.inner(), &file_hash, &target_path)
        .map_err(|error| format!("Failed to save document: {error}"))
}

#[tauri::command]
pub async fn save_audio_to_file(
    file_hash: String,
    target_path: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    media::save_attachment_to_path(state.inner(), &file_hash, &target_path)
        .map_err(|error| format!("Failed to save audio: {error}"))
}

#[tauri::command]
pub async fn list_stickers(
    state: State<'_, AppState>,
) -> Result<Vec<crate::storage::db::Sticker>, String> {
    stickers::list_stickers(state.inner()).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn add_sticker(
    file_path: String,
    state: State<'_, AppState>,
) -> Result<AddStickerResult, String> {
    stickers::add_sticker(state.inner(), &file_path).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn add_stickers_batch(
    file_paths: Vec<String>,
    state: State<'_, AppState>,
) -> Result<StickerBatchImportResult, String> {
    Ok(stickers::add_stickers_batch(state.inner(), file_paths))
}

#[tauri::command]
pub async fn delete_sticker(file_hash: String, state: State<'_, AppState>) -> Result<(), String> {
    stickers::delete_sticker(state.inner(), &file_hash).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn save_sticker_from_message(
    file_hash: String,
    state: State<'_, AppState>,
) -> Result<AddStickerResult, String> {
    stickers::save_sticker_from_message(state.inner(), &file_hash)
        .map_err(|error| error.to_string())
}

fn data_url(mime_type: &str, data: &[u8]) -> String {
    format!("data:{};base64,{}", mime_type, STANDARD.encode(data))
}

fn update_file_mime_type(app_state: &AppState, file_hash: &str, mime_type: &str) {
    if let Ok(conn) = app_state.db_conn.lock() {
        let _ = conn.execute(
            "UPDATE files SET mime_type = ?2 WHERE file_hash = ?1",
            rusqlite::params![file_hash, mime_type],
        );
    }
}
