use rust_connect::plugins::PluginEvent;

#[test]
fn test_plugin_event_battery_serialization() {
    let e = PluginEvent::Battery {
        device_id: "test".into(),
        current_charge: 85,
        is_charging: true,
    };
    let s = serde_json::to_string(&e).unwrap();
    println!("SERIALIZED: {}", s);
    assert!(s.contains("current_charge"));
}
