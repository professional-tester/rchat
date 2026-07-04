use tauri::State;

use crate::chat::envelopes;
use crate::storage;
use crate::AppState;

#[tauri::command]
pub async fn create_envelope(
    id: String,
    name: String,
    icon: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    println!(
        "[Backend] create_envelope call: {}, {}, icon: {:?}",
        id, name, icon
    );
    envelopes::create_envelope(&state, &id, &name, icon.as_deref()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_envelope(
    id: String,
    name: String,
    icon: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    envelopes::update_envelope(&state, &id, &name, icon.as_deref()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_envelope(id: String, state: State<'_, AppState>) -> Result<(), String> {
    envelopes::delete_envelope(&state, &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_envelopes(
    state: State<'_, AppState>,
) -> Result<Vec<storage::db::Envelope>, String> {
    envelopes::list_envelopes(&state).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn move_chat_to_envelope(
    chat_id: String,
    envelope_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    println!(
        "[Backend] move_chat_to_envelope: chat_id={}, envelope_id={:?}",
        chat_id, envelope_id
    );
    envelopes::move_chat_to_envelope(&state, &chat_id, envelope_id.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_envelope_assignments(
    state: State<'_, AppState>,
) -> Result<Vec<storage::db::ChatAssignment>, String> {
    envelopes::list_assignments(&state).map_err(|e| e.to_string())
}
