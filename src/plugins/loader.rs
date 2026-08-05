//! Plugin loader
//!
//! Single Responsibility: Create and register all built-in plugins.

use super::registry::PluginRegistry;
use super::{PluginAccess, PluginEventBroadcaster};
use std::sync::Arc;

pub fn load_default_plugins(
    plugin_events: Arc<PluginEventBroadcaster>,
    cert_manager: Arc<crate::protocol::crypto::CertificateManager>,
    connection_manager: Arc<crate::protocol::ConnectionManager>,
    pairing_handler: Arc<crate::protocol::pairing::PairingHandler>,
    enable_input: bool,
) -> PluginAccess {
    let mousepad = if enable_input {
        super::mousepad::MousepadPlugin::new()
    } else {
        super::mousepad::MousepadPlugin::new_without_input()
    };
    let presenter = if enable_input {
        super::PresenterPlugin::new()
    } else {
        super::PresenterPlugin::new_without_input()
    };

    PluginAccess {
        share: Arc::new(
            super::SharePlugin::new()
                .with_cert_manager(cert_manager)
                .with_connection_manager(connection_manager.clone())
                .with_events(plugin_events.clone()),
        ),
        battery: Arc::new(super::BatteryPlugin::new(plugin_events.clone())),
        sms: Arc::new(super::SmsPlugin::new()),
        // The connection manager is the fan-out path for local clipboard
        // changes; the session clipboard backend itself is enabled only at
        // the production entry point (bootstrap.rs create_state).
        clipboard: Arc::new(
            super::ClipboardPlugin::new(plugin_events.clone())
                .with_connection_manager(connection_manager.clone()),
        ),
        // Same fan-out wiring as clipboard: local player updates must reach
        // devices outside handle_packet. The session D-Bus backend itself is
        // enabled only at the production entry point (bootstrap.rs).
        mpris: Arc::new(
            super::MprisPlugin::new(plugin_events.clone())
                .with_connection_manager(connection_manager.clone()),
        ),
        notification: Arc::new(super::NotificationPlugin::new(plugin_events.clone())),
        telephony: Arc::new(super::TelephonyPlugin::new(plugin_events.clone())),
        // Session D-Bus pause backend is enabled only at the production
        // entry point (bootstrap.rs create_state), same as clipboard/mpris.
        pausemusic: Arc::new(super::PausemusicPlugin::new()),
        connectivity: Arc::new(super::ConnectivityPlugin::new()),
        sftp: Arc::new(super::SftpPlugin::with_events(plugin_events.clone())),
        mousepad: Arc::new(mousepad),
        lock: Arc::new(super::LockPlugin::new()),
        systemvolume: Arc::new(super::SystemVolumePlugin::new()),
        ping: Arc::new(super::PingPlugin::new()),
        findmyphone: Arc::new(super::FindMyPhonePlugin::new()),
        findthisdevice: Arc::new(super::FindThisDevicePlugin::new()),
        presenter: Arc::new(presenter),
        contacts: Arc::new(super::ContactsPlugin::new()),
        runcommand: Arc::new(super::RuncommandPlugin::new()),
        sendnotifications: Arc::new(
            super::SendNotificationsPlugin::new(plugin_events.clone(), pairing_handler)
                .with_connection_manager(connection_manager),
        ),
        remotekeyboard: Arc::new(super::RemoteKeyboardPlugin::new(plugin_events.clone())),
        digitizer: Arc::new(super::DigitizerPlugin::new()),
        // Session D-Bus screensaver backend is enabled only at the
        // production entry point (bootstrap.rs create_state).
        screensaver_inhibit: Arc::new(super::ScreensaverInhibitPlugin::new()),
        remotecommands: Arc::new(super::RemoteCommandsPlugin::new(plugin_events.clone())),
    }
}

pub async fn load_all(registry: &PluginRegistry, plugins: &PluginAccess) {
    for plugin in plugins.all() {
        registry.register(plugin).await;
    }
}

// Test-only helper that wires every plugin explicitly; the flat parameter list is
// intentional so each plugin's registration can be asserted individually.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub async fn load_all_with_plugins(
    registry: &PluginRegistry,
    _plugin_events: std::sync::Arc<super::PluginEventBroadcaster>,
    share_plugin: std::sync::Arc<super::SharePlugin>,
    battery_plugin: std::sync::Arc<super::BatteryPlugin>,
    sms_plugin: std::sync::Arc<super::SmsPlugin>,
    clipboard_plugin: std::sync::Arc<super::ClipboardPlugin>,
    mpris_plugin: std::sync::Arc<super::MprisPlugin>,
    notification_plugin: std::sync::Arc<super::NotificationPlugin>,
    telephony_plugin: std::sync::Arc<super::TelephonyPlugin>,
    pairing_handler: std::sync::Arc<crate::protocol::pairing::PairingHandler>,
) {
    use super::contacts::ContactsPlugin;
    use super::findmyphone::FindMyPhonePlugin;
    use super::lock::LockPlugin;
    use super::mousepad::MousepadPlugin;
    use super::pausemusic::PausemusicPlugin;
    use super::ping::PingPlugin;
    use super::plugin::Plugin;
    use super::presenter::PresenterPlugin;
    use super::runcommand::RuncommandPlugin;
    use super::systemvolume::SystemVolumePlugin;

    let plugins: Vec<std::sync::Arc<dyn Plugin>> = vec![
        std::sync::Arc::new(PingPlugin::new()),
        battery_plugin,
        notification_plugin,
        sms_plugin,
        clipboard_plugin,
        share_plugin,
        mpris_plugin,
        telephony_plugin,
        std::sync::Arc::new(PausemusicPlugin::new()),
        std::sync::Arc::new(MousepadPlugin::new_without_input()),
        std::sync::Arc::new(LockPlugin::new()),
        std::sync::Arc::new(SystemVolumePlugin::new()),
        std::sync::Arc::new(FindMyPhonePlugin::new()),
        std::sync::Arc::new(PresenterPlugin::new_without_input()),
        std::sync::Arc::new(ContactsPlugin::new()),
        std::sync::Arc::new(RuncommandPlugin::new()),
        std::sync::Arc::new(super::sendnotifications::SendNotificationsPlugin::new(
            _plugin_events.clone(),
            pairing_handler.clone(),
        )),
        std::sync::Arc::new(super::remotekeyboard::RemoteKeyboardPlugin::new(
            _plugin_events.clone(),
        )),
        std::sync::Arc::new(super::digitizer::DigitizerPlugin::new()),
        std::sync::Arc::new(super::remotecommands::RemoteCommandsPlugin::new(
            _plugin_events.clone(),
        )),
    ];

    for plugin in plugins {
        registry.register(plugin).await;
    }
}
