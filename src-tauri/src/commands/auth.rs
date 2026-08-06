use tauri::{Emitter, Manager, State};

use crate::settings::connectivity;
use crate::storage::config::{Config, ConnectivityMode, ConnectivitySettings};
use crate::{network, oauth, AppState, NetworkState};
use std::sync::Arc;

#[derive(serde::Serialize)]
pub struct AuthStatus {
    is_setup: bool,
    is_unlocked: bool,
    is_github_connected: bool,
    is_online: bool,
    connectivity: ConnectivitySettings,
}

pub use crate::settings::connectivity::ConnectivitySettingsPatch;

fn unlocked_auth_status(config: &Config) -> AuthStatus {
    let connectivity = connectivity::normalize_connectivity(config.user.connectivity.clone());
    AuthStatus {
        is_setup: true,
        is_unlocked: true,
        is_github_connected: config.system.github_token.is_some(),
        is_online: connectivity.github_sync_enabled,
        connectivity,
    }
}

#[tauri::command]
pub async fn save_api_token(token: String, state: State<'_, AppState>) -> Result<(), String> {
    // Fetch username from GitHub API using octocrab
    let octocrab = octocrab::Octocrab::builder()
        .personal_token(token.clone())
        .build()
        .map_err(|e| format!("Failed to build octocrab client: {}", e))?;

    let user: octocrab::models::Author = octocrab
        .get("/user", None::<&()>)
        .await
        .map_err(|e| format!("Failed to fetch GitHub user: {}", e))?;

    let username = user.login;
    println!("[Backend] GitHub username fetched: {}", username);

    // Save both token and username
    let mgr = state.config_manager.lock().await;
    let mut config = mgr.load().await.map_err(|e| e.to_string())?;
    config.system.github_token = Some(token);
    config.system.github_username = Some(username);
    mgr.save(&config).await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn check_auth_status(state: State<'_, AppState>) -> Result<AuthStatus, String> {
    let mgr = state.config_manager.lock().await;

    let connectivity = if mgr.is_unlocked() {
        if let Ok(config) = mgr.load().await {
            connectivity::normalize_connectivity(config.user.connectivity)
        } else {
            ConnectivitySettings::default()
        }
    } else {
        ConnectivitySettings::default()
    };
    let is_online = connectivity.github_sync_enabled;

    // Migration: if token exists but username is missing, fetch and save it
    if mgr.is_unlocked() {
        if let Ok(config) = mgr.load().await {
            if config.system.github_token.is_some() && config.system.github_username.is_none() {
                if let Some(ref token) = config.system.github_token {
                    if let Ok(octocrab) = octocrab::Octocrab::builder()
                        .personal_token(token.clone())
                        .build()
                    {
                        if let Ok(user) = octocrab
                            .get::<octocrab::models::Author, _, _>("/user", None::<&()>)
                            .await
                        {
                            println!(
                                "[Backend] Migrating: fetched GitHub username {}",
                                user.login
                            );
                            let mut updated_config = config.clone();
                            updated_config.system.github_username = Some(user.login);
                            let _ = mgr.save(&updated_config).await;
                        }
                    }
                }
            }
        }
    }

    Ok(AuthStatus {
        is_setup: mgr.exists(),
        is_unlocked: mgr.is_unlocked(),
        is_github_connected: mgr.has_token().await,
        is_online,
        connectivity,
    })
}

#[tauri::command]
pub async fn get_connectivity_settings(
    state: State<'_, AppState>,
) -> Result<ConnectivitySettings, String> {
    connectivity::get_connectivity_settings(&state)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn set_connectivity_mode(
    mode: ConnectivityMode,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<ConnectivitySettings, String> {
    let runtime = app_handle.try_state::<NetworkState>();
    connectivity::set_connectivity_mode(&state, runtime.as_ref().map(|value| value.inner()), mode)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn update_connectivity_settings(
    patch: ConnectivitySettingsPatch,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<ConnectivitySettings, String> {
    let runtime = app_handle.try_state::<NetworkState>();
    connectivity::update_connectivity_settings(
        &state,
        runtime.as_ref().map(|value| value.inner()),
        patch,
    )
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn init_vault(
    password: String,
    state: State<'_, AppState>,
) -> Result<AuthStatus, String> {
    let mut mgr = state.config_manager.lock().await;
    let config = mgr.init(password.trim()).await.map_err(|e| e.to_string())?;
    Ok(unlocked_auth_status(&config))
}

#[tauri::command]
pub async fn unlock_vault(
    password: String,
    state: State<'_, AppState>,
) -> Result<AuthStatus, String> {
    let mut mgr = state.config_manager.lock().await;
    let config = mgr
        .unlock_with_password(password.trim())
        .await
        .map_err(|e| {
            eprintln!("[Backend] Unlock failed: {}", e);
            e.to_string()
        })?;
    println!("[Backend] Vault unlocked successfully.");
    Ok(unlocked_auth_status(&config))
}

/// Start the P2P network - call this AFTER vault is unlocked
/// This ensures the persisted keypair can be loaded from the encrypted config
#[tauri::command]
pub async fn start_network(app_handle: tauri::AppHandle) -> Result<(), String> {
    println!("[Backend] start_network called (post-unlock)");

    // Check if network is already running
    if app_handle.try_state::<NetworkState>().is_some() {
        println!("[Backend] Network already initialized, skipping...");
        return Ok(());
    }

    {
        let app_state = app_handle.state::<AppState>();
        let github_peer_mapping = {
            let mgr = app_state.config_manager.lock().await;
            let config = mgr.load().await.map_err(|e| e.to_string())?;
            config.user.github_peer_mapping
        };
        let mut conn = app_state.db_conn.lock().map_err(|e| e.to_string())?;
        crate::storage::db::migrate_legacy_github_chat_ids(&mut conn, &github_peer_mapping)
            .map_err(|e| e.to_string())?;
    }

    let core_app_state = app_handle.state::<AppState>().inner().clone();
    let event_sink = Arc::new(crate::event_sink::TauriEventSink::new(app_handle.clone()));

    match network::start(core_app_state, event_sink).await {
        Ok(network_state) => {
            app_handle.manage(network_state);
            println!("[Backend] Network started successfully!");
            let _ = app_handle.emit("auth-status", serde_json::json!({"unlocked": true}));
            Ok(())
        }
        Err(e) => {
            eprintln!("[Backend] Failed to start network: {}", e);
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub async fn start_github_auth() -> Result<oauth::AuthState, String> {
    oauth::start_device_flow().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn poll_github_auth(device_code: String) -> Result<String, String> {
    oauth::poll_for_token(&device_code)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn reset_vault(state: State<'_, AppState>) -> Result<(), String> {
    let mut mgr = state.config_manager.lock().await;
    mgr.reset().await.map_err(|e| e.to_string())?;
    Ok(())
}
