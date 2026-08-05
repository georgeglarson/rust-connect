# Rust Connect Architecture

## Overview

Rust Connect is designed as a layered architecture with clear separation of concerns:

```
┌─────────────────────────────────────────────────────────────┐
│                    Frontend Layer                            │
│  (Web UI, Desktop App, CLI, Mobile Web, etc.)               │
└────────────────────────┬────────────────────────────────────┘
                         │ HTTP/WebSocket
┌────────────────────────▼────────────────────────────────────┐
│                     API Layer                                │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ REST Routes  │  │  WebSocket   │  │     Auth     │      │
│  │   (Axum)     │  │   Handler    │  │  (API Keys)  │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
└────────────────────────┬────────────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────────────┐
│                  Device Manager                              │
│  - Device lifecycle management                               │
│  - Connection state tracking                                 │
│  - Event broadcasting                                        │
└────────────────────────┬────────────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────────────┐
│                 Protocol Layer                               │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │  Discovery   │  │  Connection  │  │   Pairing    │      │
│  │ (UDP Bcast)  │  │ (TCP/TLS)    │  │ (RSA Keys)   │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
│  ┌──────────────┐  ┌──────────────┐                         │
│  │    Packet    │  │    Crypto    │                         │
│  │ Serializer   │  │  (Certs)     │                         │
│  └──────────────┘  └──────────────┘                         │
└────────────────────────┬────────────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────────────┐
│                   Plugin System                              │
│  ┌─────┐ ┌─────┐ ┌─────┐ ┌─────┐ ┌─────┐ ┌─────┐          │
│  │ SMS │ │Notif│ │Clip │ │Share│ │Batt │ │MPRIS│ ...      │
│  └─────┘ └─────┘ └─────┘ └─────┘ └─────┘ └─────┘          │
└─────────────────────────────────────────────────────────────┘
```

## Layer Details

### 1. Frontend Layer

**Responsibility:** User interface and experience

**Technologies:** Any (React, Svelte, GTK, Qt, Terminal UI, etc.)

**Communication:** HTTP REST + WebSocket

**Key Points:**
- Completely decoupled from backend
- Multiple frontends can coexist
- Can be developed independently
- No direct protocol knowledge needed

### 2. API Layer

**Responsibility:** HTTP interface and authentication

**Components:**

#### REST API (Axum)
```rust
// Example route structure
Router::new()
    .route("/api/v1/devices", get(list_devices))
    .route("/api/v1/devices/:id", get(get_device))
    .route("/api/v1/devices/:id/sms", post(send_sms))
    .layer(AuthLayer::new())
```

**Features:**
- RESTful design
- JSON request/response
- API key authentication
- Rate limiting
- CORS support
- OpenAPI documentation

#### WebSocket Handler
```rust
// Real-time event streaming
async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}
```

**Events:**
- Device connected/disconnected
- Notification received
- SMS received
- Battery changed
- Clipboard updated

#### Authentication
- API key based (simple, stateless)
- Optional JWT for advanced use cases
- Per-device permissions
- Rate limiting per key

### 3. Device Manager

**Responsibility:** Device lifecycle and state management

**Key Structures:**

```rust
pub struct DeviceManager {
    devices: Arc<RwLock<HashMap<DeviceId, Device>>>,
    event_tx: broadcast::Sender<DeviceEvent>,
    protocol: Arc<ProtocolHandler>,
}

pub struct Device {
    id: DeviceId,
    name: String,
    device_type: DeviceType,
    state: DeviceState,
    plugins: HashMap<String, Box<dyn Plugin>>,
    connection: Option<Connection>,
}

pub enum DeviceState {
    Discovered,
    Pairing,
    Paired,
    Connected,
    Disconnected,
}
```

**Responsibilities:**
- Track all known devices
- Manage device state transitions
- Route packets to appropriate plugins
- Broadcast events to API layer
- Handle reconnection logic

### 4. Protocol Layer

**Responsibility:** KDE Connect protocol implementation

#### Discovery (UDP)

```rust
pub struct DiscoveryService {
    socket: UdpSocket,
    identity: Identity,
}

// Broadcasts identity packet every 5 seconds
// Listens for incoming identity packets
// Port: 1716 (UDP)
```

**Identity Packet:**
```json
{
  "id": 1234567890,
  "type": "kdeconnect.identity",
  "body": {
    "deviceId": "abc123...",
    "deviceName": "My Phone",
    "deviceType": "phone",
    "protocolVersion": 7,
    "incomingCapabilities": ["kdeconnect.sms.messages", ...],
    "outgoingCapabilities": ["kdeconnect.sms.request", ...]
  }
}
```

#### Connection (TCP/TLS)

```rust
pub struct Connection {
    stream: TlsStream<TcpStream>,
    device_id: DeviceId,
    reader: PacketReader,
    writer: PacketWriter,
}

// Ports: 1716-1764 (TCP)
// TLS 1.2+ required
// Certificate-based authentication
```

**Connection Flow:**
1. TCP connection established
2. TLS handshake (mutual authentication)
3. Identity packet exchange
4. Capability negotiation
5. Plugin initialization

#### Pairing

```rust
pub struct PairingHandler {
    cert_manager: CertificateManager,
    pending_pairs: HashMap<DeviceId, PairingRequest>,
}

// RSA key exchange
// 30-minute timeout
// User confirmation required on both sides
```

**Pairing Flow:**
1. Device A sends pair request
2. Device B shows notification
3. User accepts on Device B
4. Device B sends pair response
5. Both devices store public keys
6. Connection upgraded to "paired"

#### Packet Serialization

```rust
#[derive(Serialize, Deserialize)]
pub struct Packet {
    pub id: i64,
    #[serde(rename = "type")]
    pub packet_type: String,
    pub body: serde_json::Value,
}

// All packets are JSON
// Newline-delimited over TCP
// Max size: 512KB (configurable)
```

#### Crypto

```rust
pub struct CertificateManager {
    cert_path: PathBuf,
    key_path: PathBuf,
    device_id: DeviceId,
}

// Certificate = Device ID (common name)
// Self-signed certificates
// RSA 2048-bit keys
// Stored in ~/.config/rust-connect/
```

### 5. Plugin System

**Responsibility:** Feature implementation

**Plugin Trait:**

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;
    fn incoming_capabilities(&self) -> &[&str];
    fn outgoing_capabilities(&self) -> &[&str];
    
    async fn handle_packet(&mut self, packet: Packet) -> Result<()>;
    async fn on_connected(&mut self) -> Result<()>;
    async fn on_disconnected(&mut self) -> Result<()>;
}
```

**Plugin Lifecycle:**
1. **Registration:** Plugin registers capabilities
2. **Initialization:** Called when device connects
3. **Packet Handling:** Receives matching packets
4. **Cleanup:** Called when device disconnects

**Example Plugin (Ping):**

```rust
pub struct PingPlugin {
    device_id: DeviceId,
    event_tx: mpsc::Sender<PluginEvent>,
}

#[async_trait]
impl Plugin for PingPlugin {
    fn name(&self) -> &str { "ping" }
    
    fn incoming_capabilities(&self) -> &[&str] {
        &["kdeconnect.ping"]
    }
    
    fn outgoing_capabilities(&self) -> &[&str] {
        &["kdeconnect.ping"]
    }
    
    async fn handle_packet(&mut self, packet: Packet) -> Result<()> {
        if packet.packet_type == "kdeconnect.ping" {
            // Handle ping
            self.event_tx.send(PluginEvent::PingReceived).await?;
        }
        Ok(())
    }
}
```

## Data Flow

### Incoming Packet Flow

```
Android Device
    │
    ├─ TCP/TLS ─────────────────────────────────┐
    │                                            │
    ▼                                            ▼
Connection                                  Protocol Layer
    │                                            │
    ├─ Deserialize ─────────────────────────────┤
    │                                            │
    ▼                                            ▼
Device Manager                              Packet Router
    │                                            │
    ├─ Route by type ───────────────────────────┤
    │                                            │
    ▼                                            ▼
Plugin (SMS/Notification/etc)              Handle Packet
    │                                            │
    ├─ Process ─────────────────────────────────┤
    │                                            │
    ▼                                            ▼
Event Broadcast                             API Layer
    │                                            │
    ├─ WebSocket ───────────────────────────────┤
    │                                            │
    ▼                                            ▼
Frontend                                    Update UI
```

### Outgoing Request Flow

```
Frontend (Web UI)
    │
    ├─ HTTP POST /api/v1/devices/:id/sms ──────┐
    │                                            │
    ▼                                            ▼
API Layer                                   Auth Check
    │                                            │
    ├─ Validate ────────────────────────────────┤
    │                                            │
    ▼                                            ▼
Device Manager                              Find Device
    │                                            │
    ├─ Get Plugin ──────────────────────────────┤
    │                                            │
    ▼                                            ▼
SMS Plugin                                  Create Packet
    │                                            │
    ├─ Serialize ───────────────────────────────┤
    │                                            │
    ▼                                            ▼
Connection                                  Send via TCP/TLS
    │                                            │
    ▼                                            ▼
Android Device                              Receive & Process
```

## Concurrency Model

### Async Runtime: Tokio

```rust
#[tokio::main]
async fn main() {
    // Spawn background tasks
    tokio::spawn(discovery_service());
    tokio::spawn(connection_manager());
    tokio::spawn(api_server());
    
    // Wait for shutdown signal
    tokio::signal::ctrl_c().await.unwrap();
}
```

### Task Structure

1. **Discovery Task:** UDP broadcast/listen loop
2. **Connection Tasks:** One per device connection
3. **API Server Task:** HTTP/WebSocket server
4. **Plugin Tasks:** Per-plugin event processing
5. **Event Broadcaster:** Distributes events to subscribers

### Synchronization

```rust
// Shared state with Arc + RwLock
type SharedDevices = Arc<RwLock<HashMap<DeviceId, Device>>>;

// Event broadcasting with tokio::sync::broadcast
let (tx, _rx) = broadcast::channel(100);

// Plugin communication with mpsc channels
let (plugin_tx, plugin_rx) = mpsc::channel(32);
```

## Error Handling

### Error Types

```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Device not found: {0}")]
    DeviceNotFound(DeviceId),
    
    #[error("Connection error: {0}")]
    Connection(#[from] std::io::Error),
    
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),
    
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    
    #[error("Plugin error: {0}")]
    Plugin(String),
}

pub type Result<T> = std::result::Result<T, Error>;
```

### Error Propagation

- Use `?` operator for propagation
- Log errors at appropriate levels
- Return meaningful error responses in API
- Graceful degradation (plugin failure doesn't crash daemon)

## Configuration

### Config File Structure

```toml
# ~/.config/rust-connect/config.toml

[server]
host = "127.0.0.1"
port = 8080
api_key = "your-secret-key"

[protocol]
discovery_port = 1716
tcp_port_min = 1716
tcp_port_max = 1764
broadcast_interval = 5

[device]
name = "My Computer"
type = "desktop"

[plugins]
enabled = ["sms", "notification", "battery", "share"]

[logging]
level = "info"
file = "~/.local/share/rust-connect/rust-connect.log"
```

### Runtime Configuration

```rust
pub struct Config {
    pub server: ServerConfig,
    pub protocol: ProtocolConfig,
    pub device: DeviceConfig,
    pub plugins: PluginConfig,
    pub logging: LoggingConfig,
}

// Load from file or environment variables
// Hot-reload support for some settings
```

## Security Considerations

### TLS/Certificate Management
- Self-signed certificates (no CA needed)
- Certificate = Device ID (prevents impersonation)
- Mutual authentication required
- TLS 1.2+ only

### API Security
- API key authentication
- Rate limiting per key
- CORS configuration
- Input validation
- SQL injection prevention (if using DB)

### Network Security
- Local network only by default
- Optional remote access with VPN/tunnel
- Firewall-friendly (configurable ports)
- No cloud dependencies

## Performance Considerations

### Memory Usage
- Target: <50 MB resident memory
- Lazy plugin loading
- Connection pooling
- Efficient packet buffering

### CPU Usage
- Async I/O (no blocking)
- Minimal packet processing overhead
- Efficient JSON serialization
- Background task scheduling

### Network Usage
- Small packet sizes (JSON)
- Compression for large transfers
- Efficient discovery (5s intervals)
- Connection keepalive

## Testing Strategy

### Unit Tests
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_packet_serialization() { }
    
    #[test]
    fn test_device_state_transitions() { }
}
```

### Integration Tests
```rust
#[tokio::test]
async fn test_device_pairing() { }

#[tokio::test]
async fn test_sms_send_receive() { }
```

### Protocol Tests
- Test with real Android device
- Packet capture and replay
- Compatibility testing

## Deployment

### Systemd Service

```ini
[Unit]
Description=Rust Connect Daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/rust-connect
Restart=on-failure
User=%u

[Install]
WantedBy=default.target
```

### Packaging
- Flatpak (sandboxed)
- AppImage (portable)
- .deb (Debian/Ubuntu)
- .rpm (Fedora/RHEL)
- AUR (Arch Linux)

## Future Enhancements

### Phase 2+
- Bluetooth support
- Cloud sync (optional)
- Multi-device support
- Plugin marketplace
- Mobile app (iOS/Android)
- Windows/macOS support

### Performance
- Zero-copy packet processing
- Connection multiplexing
- Batch operations
- Caching layer

### Features
- End-to-end encryption
- File sync
- Remote desktop
- Voice/video calls
