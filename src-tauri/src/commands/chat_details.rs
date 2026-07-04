use tauri::State;

use crate::chat::details;
use crate::{AppState, NetworkState};

#[tauri::command]
pub async fn get_chat_details_overview(
    chat_id: String,
    app_state: State<'_, AppState>,
    net_state: State<'_, NetworkState>,
) -> Result<details::ChatDetailsOverview, String> {
    details::overview(&app_state, &net_state, &chat_id)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_chat_stats(
    chat_id: String,
    app_state: State<'_, AppState>,
) -> Result<details::ChatStats, String> {
    details::stats(&app_state, &chat_id).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn list_chat_files(
    chat_id: String,
    filter: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
    app_state: State<'_, AppState>,
) -> Result<Vec<crate::storage::db::ChatFileRow>, String> {
    details::files(&app_state, &chat_id, filter.as_deref(), limit, offset)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn drop_chat_connection(
    chat_id: String,
    _app_state: State<'_, AppState>,
    net_state: State<'_, NetworkState>,
) -> Result<(), String> {
    details::drop_connection(&net_state, &chat_id)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn force_chat_reconnect(
    chat_id: String,
    _app_state: State<'_, AppState>,
    net_state: State<'_, NetworkState>,
) -> Result<(), String> {
    details::force_reconnect(&net_state, &chat_id)
        .await
        .map_err(|error| error.to_string())
}
