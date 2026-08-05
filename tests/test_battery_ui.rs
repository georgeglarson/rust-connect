use rust_connect::plugins::events::PluginEvent;

#[tokio::test]
async fn test_battery_ui_payload() {
    let e = PluginEvent::Battery {
        device_id: "test".into(),
        current_charge: 85,
        is_charging: true,
    };
    let json = serde_json::to_value(&e).unwrap();
    println!("SSE Event JSON: {}", json);

    // Check if the current_charge field is present in the UI's expected format
    assert!(json.get("current_charge").is_some());
    assert!(json.get("is_charging").is_some());
}
