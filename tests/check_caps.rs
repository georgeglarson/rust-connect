//! Check identity capabilities
use rust_connect::app::AppState;
use rust_connect::config::settings::AppSettings;
use std::sync::Arc;

#[tokio::test]
async fn check_identity_capabilities() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let settings = AppSettings::default().with_data_dir(temp_dir.path().to_path_buf());
    let state = Arc::new(AppState::new_without_input(settings).unwrap());
    state.init_plugins().await;

    let mut incoming_caps: Vec<String> = Vec::new();
    for plugin_name in state.plugin_registry.list().await {
        if let Some(plugin) = state.plugin_registry.get(&plugin_name).await {
            incoming_caps.extend(plugin.incoming_capabilities());
        }
    }

    println!("INCOMING CAPS: {:?}", incoming_caps);
    assert!(incoming_caps.contains(&"kdeconnect.clipboard".to_string()));
    assert!(incoming_caps.contains(&"kdeconnect.clipboard.connect".to_string()));
}
