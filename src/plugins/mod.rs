//! Plugin System
//!
//! This module provides the plugin system for feature implementation.
//! Each plugin has a single responsibility (one feature).
//! - plugin: Plugin trait definition
//! - registry: Plugin storage and lookup by capability
//! - loader: Create and register all built-in plugins

pub mod battery;
pub mod clipboard;
pub mod connectivity;
pub mod contacts;
pub mod digitizer;
pub mod events;
pub mod findmyphone;
pub mod findthisdevice;
pub mod loader;
pub mod lock;
pub mod mousepad;
pub mod mpris;
pub mod notification;
pub mod pausemusic;
pub mod ping;
pub mod plugin;
pub mod presenter;
pub mod registry;
pub mod remotecommands;
pub mod remotekeyboard;
pub mod runcommand;
pub mod screensaver_inhibit;
pub mod sendnotifications;
pub mod sftp;
pub mod share;
pub mod shareinputdevices;
pub mod sms;
pub mod systemvolume;
pub mod telephony;

use std::sync::Arc;

pub use battery::BatteryPlugin;
pub use clipboard::ClipboardPlugin;
pub use connectivity::ConnectivityPlugin;
pub use contacts::ContactsPlugin;
pub use digitizer::DigitizerPlugin;
pub use events::{PluginEvent, PluginEventBroadcaster};
pub use findmyphone::FindMyPhonePlugin;
pub use findthisdevice::FindThisDevicePlugin;
pub use loader::{load_all, load_default_plugins};
pub use lock::LockPlugin;
pub use mpris::MprisPlugin;
pub use notification::NotificationPlugin;
pub use pausemusic::PausemusicPlugin;
pub use ping::PingPlugin;
pub use plugin::Plugin;
pub use presenter::PresenterPlugin;
pub use registry::PluginRegistry;
pub use remotecommands::RemoteCommandsPlugin;
pub use remotekeyboard::RemoteKeyboardPlugin;
pub use runcommand::RuncommandPlugin;
pub use screensaver_inhibit::ScreensaverInhibitPlugin;
pub use sendnotifications::SendNotificationsPlugin;
pub use sftp::SftpPlugin;
pub use share::SharePlugin;
pub use sms::SmsPlugin;
pub use systemvolume::SystemVolumePlugin;
pub use telephony::TelephonyPlugin;

/// Typed accessor for all plugin instances.
/// Replaces individual plugin fields on AppState.
pub struct PluginAccess {
    pub share: Arc<SharePlugin>,
    pub battery: Arc<BatteryPlugin>,
    pub sms: Arc<SmsPlugin>,
    pub clipboard: Arc<ClipboardPlugin>,
    pub mpris: Arc<MprisPlugin>,
    pub notification: Arc<NotificationPlugin>,
    pub telephony: Arc<TelephonyPlugin>,
    pub pausemusic: Arc<PausemusicPlugin>,
    pub connectivity: Arc<ConnectivityPlugin>,
    pub sftp: Arc<SftpPlugin>,
    pub mousepad: Arc<crate::plugins::mousepad::MousepadPlugin>,
    pub lock: Arc<LockPlugin>,
    pub systemvolume: Arc<SystemVolumePlugin>,
    pub ping: Arc<PingPlugin>,
    pub findmyphone: Arc<FindMyPhonePlugin>,
    pub findthisdevice: Arc<FindThisDevicePlugin>,
    pub presenter: Arc<PresenterPlugin>,
    pub contacts: Arc<ContactsPlugin>,
    pub runcommand: Arc<RuncommandPlugin>,
    pub sendnotifications: Arc<SendNotificationsPlugin>,
    pub remotekeyboard: Arc<RemoteKeyboardPlugin>,
    pub digitizer: Arc<DigitizerPlugin>,
    pub screensaver_inhibit: Arc<ScreensaverInhibitPlugin>,
    pub remotecommands: Arc<RemoteCommandsPlugin>,
}

impl PluginAccess {
    /// Returns all plugins as a vector of Arc<dyn Plugin>.
    pub fn all(&self) -> Vec<Arc<dyn Plugin>> {
        vec![
            self.ping.clone(),
            self.battery.clone(),
            self.notification.clone(),
            self.sms.clone(),
            self.clipboard.clone(),
            self.share.clone(),
            self.mpris.clone(),
            self.telephony.clone(),
            self.pausemusic.clone(),
            self.connectivity.clone(),
            self.sftp.clone(),
            self.mousepad.clone(),
            self.lock.clone(),
            self.systemvolume.clone(),
            self.findmyphone.clone(),
            self.findthisdevice.clone(),
            self.presenter.clone(),
            self.contacts.clone(),
            self.runcommand.clone(),
            self.sendnotifications.clone(),
            self.remotekeyboard.clone(),
            self.digitizer.clone(),
            self.screensaver_inhibit.clone(),
            self.remotecommands.clone(),
        ]
    }
}
