use crate::AppState;
use anyhow::Result;
pub use rchat_video_capture::CaptureDeviceInfo;
use rchat_video_capture::VideoCaptureError;

pub fn list_camera_devices() -> Result<Vec<CaptureDeviceInfo>, VideoCaptureError> {
    rchat_video_capture::list_devices()
}

pub async fn get_selected_camera_device_id(app_state: &AppState) -> Result<Option<String>> {
    let manager = app_state.config_manager.lock().await;
    Ok(manager.load().await?.user.selected_camera_device_id)
}

pub async fn set_selected_camera_device_id(
    app_state: &AppState,
    device_id: Option<String>,
) -> Result<()> {
    let manager = app_state.config_manager.lock().await;
    let mut config = manager.load().await?;
    config.user.selected_camera_device_id = device_id
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    manager.save(&config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::profile::test_app_state;

    #[tokio::test]
    async fn camera_device_preference_round_trips_and_normalizes_automatic() {
        let (_temp, app_state) = test_app_state().await;

        assert!(get_selected_camera_device_id(&app_state)
            .await
            .expect("initial preference")
            .is_none());

        set_selected_camera_device_id(&app_state, Some("  camera-b  ".to_string()))
            .await
            .expect("save camera preference");
        assert_eq!(
            get_selected_camera_device_id(&app_state)
                .await
                .expect("saved preference")
                .as_deref(),
            Some("camera-b")
        );

        set_selected_camera_device_id(&app_state, Some("   ".to_string()))
            .await
            .expect("clear camera preference");
        assert!(get_selected_camera_device_id(&app_state)
            .await
            .expect("automatic preference")
            .is_none());
    }
}
