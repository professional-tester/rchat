use tauri::State;

use crate::settings::{peers, profile, theme as theme_settings};
use crate::storage::config::{FriendConfig, ThemeConfig, UserProfile};
use crate::AppState;

pub use crate::settings::theme::PresetInfo;

#[tauri::command]
pub async fn get_trusted_peers(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    peers::get_trusted_peers(&state).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn delete_peer(peer_id: String, state: State<'_, AppState>) -> Result<(), String> {
    peers::delete_peer(&state, &peer_id).map_err(|error| error.to_string())?;
    println!("[Backend] Deleted peer: {}", peer_id);
    Ok(())
}

#[tauri::command]
pub async fn get_friends(state: State<'_, AppState>) -> Result<Vec<FriendConfig>, String> {
    println!("[Backend] get_friends called");
    peers::get_friends(&state)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_peer_aliases(
    state: State<'_, AppState>,
) -> Result<std::collections::HashMap<String, String>, String> {
    println!("[Backend] get_peer_aliases called");
    peers::get_peer_aliases(&state).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn add_friend(
    username: String,
    x25519_key: Option<String>,
    ed25519_key: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    peers::add_friend(&state, username, x25519_key, ed25519_key)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn remove_friend(username: String, state: State<'_, AppState>) -> Result<(), String> {
    peers::remove_friend(&state, &username)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_user_profile(state: State<'_, AppState>) -> Result<UserProfile, String> {
    println!("[Backend] get_user_profile called");
    profile::get_user_profile(&state)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn update_user_profile(
    alias: Option<String>,
    avatar_path: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    profile::update_user_profile(&state, alias, avatar_path)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_pinned_peers(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    peers::get_pinned_peers(&state)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn toggle_pin_peer(username: String, state: State<'_, AppState>) -> Result<bool, String> {
    peers::toggle_pin_peer(&state, username)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_theme(state: State<'_, AppState>) -> Result<ThemeConfig, String> {
    println!("[Backend] get_theme called");
    theme_settings::get_theme(&state)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn update_theme(theme: ThemeConfig, state: State<'_, AppState>) -> Result<(), String> {
    println!("[Backend] update_theme called");
    theme_settings::update_theme(&state, theme)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn generate_simple_theme(
    primary: String,
    secondary: String,
    text: String,
) -> Result<ThemeConfig, String> {
    theme_settings::generate_simple_theme(&primary, &secondary, &text)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn list_theme_presets(state: State<'_, AppState>) -> Result<Vec<PresetInfo>, String> {
    println!("[Backend] list_theme_presets called");
    theme_settings::list_theme_presets(&state)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn apply_preset(name: String, state: State<'_, AppState>) -> Result<ThemeConfig, String> {
    println!("[Backend] apply_preset called with: {}", name);
    theme_settings::apply_preset(&state, &name)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn create_custom_theme(
    name: String,
    description: Option<String>,
    theme: ThemeConfig,
    state: State<'_, AppState>,
) -> Result<PresetInfo, String> {
    theme_settings::create_custom_theme(&state, name, description, theme)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn update_custom_theme(
    key: String,
    name: String,
    description: Option<String>,
    theme: ThemeConfig,
    state: State<'_, AppState>,
) -> Result<PresetInfo, String> {
    theme_settings::update_custom_theme(&state, key, name, description, theme)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn delete_custom_theme(key: String, state: State<'_, AppState>) -> Result<(), String> {
    theme_settings::delete_custom_theme(&state, &key)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_selected_preset(state: State<'_, AppState>) -> Result<Option<String>, String> {
    theme_settings::get_selected_preset(&state)
        .await
        .map_err(|error| error.to_string())
}
