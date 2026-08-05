# Rust-Connect Troubleshooting UI

## Purpose

This UI serves as the **internal troubleshooting interface** for rust-connect. It provides direct access to all device APIs, plugin features, and live event streams for debugging and development purposes.

## Who Uses This

- **Developers** debugging connectivity or protocol issues
- **QA** verifying all features work correctly
- **Users** experiencing issues who need to manually test capabilities

## Design Philosophy

This is deliberately NOT a polished user-facing UI. It's a technical interface that makes everything visible and accessible. If something doesn't work in this UI, the underlying API is broken. Other frontends (web, mobile, desktop) can reference this UI as the source of truth for expected behavior.

## Features Exposed

| Tab | What It Tests |
|-----|---------------|
| **Device Management** | Pair/Unpair, Connect/Disconnect, Ping, Lock |
| **Notifications** | View received notifications, send test notifications |
| **Battery** | Current charge state, request updates |
| **SMS** | Fetch threads, send messages |
| **Clipboard** | Push to phone, pull from phone (with protocol notes) |
| **Media Controls** | MPRIS player actions (Play/Pause/Stop/Prev/Next) |
| **Telephony** | Call/SMS event log from phone |
| **File Transfer** | Share files, SFTP mount/unmount |
| **System Volume** | Set volume level |
| **Live Event Stream** | Real-time SSE events from daemon |

## Key Notes

- **Clipboard pull is disabled** for background security on Android - see the note in UI
- **SFTP workflow**: Request → Wait for response → Mount
- **Telephony is read-only** - events come from the phone automatically
- **Idle timeout**: Disabled by default (`--idle-timeout 0`); use `--idle-timeout 300` to set a timeout

## Accessing

The UI runs at `http://localhost:9090/ui/` when the daemon is running with API enabled.

## Extending

When adding new plugin APIs:
1. Add handler in `src/api/handlers/plugins/`
2. Add UI section in `index.html` with refresh/action buttons
3. Add SSE event handling if the plugin broadcasts updates
4. Verify fields match what the Rust handlers expect (watch for snake_case vs camelCase)
