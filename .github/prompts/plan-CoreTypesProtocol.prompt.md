# Plan: 核心类型与协议

> 隶属于 `plan-MetaMystiaNetwork` 总纲 — 阶段 1：基础层

本计划覆盖项目依赖配置和最底层的类型/协议定义，所有上层模块均依赖这些内容。

注意：文档同时显示 Rust 内部名（snake_case）与 FFI 导出名（CamelCase），并在需要时标明对应关系（例如 `get_peer_list` ↔ `GetPeerList`）。

---

## Steps

### 1. 更新 Cargo.toml 依赖

添加运行时依赖：

- `tokio = { version = "1", features = ["rt-multi-thread", "net", "time", "sync", "macros", "io-util"] }` — 异步 TCP IO + 定时器
- `tokio-util = { version = "0.7", features = ["codec", "sync"] }` — 编解码框架（用于 `Framed`）与 `CancellationToken`。注意：crate 名称为 `tokio-util`，在代码中以 `tokio_util` 命名空间引用；本项目使用自定义 `PacketCodec` 实现粘包/分帧。
- `bytes = "1"` — PacketBuffer 使用 BytesMut
- `futures-util = { version = "0.3", features = ["sink"] }` — `SinkExt`/`StreamExt` 用于 Framed 读写
- `hostname = "0.4"` — mDNS 服务注册时获取本机主机名
- `socket2 = "0.6"` — 设置 TCP Keep-alive
- `mdns-sd = { version = "0.18", features = ["async"], default-features = false }` — 局域网服务发现
- `lz4_flex = { version = "0.12", default-features = false }` — LZ4 压缩/解压
- `postcard = { version = "1.1", features = ["alloc"], default-features = false }` — 二进制序列化
- `serde = { version = "1", features = ["derive"] }` — 序列化框架
- `rand = { version = "0.9", features = ["std", "thread_rng"], default-features = false }` — 选举超时随机化
- `tracing = "0.1"` — 日志
- `tracing-subscriber = { version = "0.3", features = ["env-filter"], optional = true }` — 日志后端
- `parking_lot = "0.12"` — 更高效的 Mutex/RwLock

添加构建依赖（`[build-dependencies]`）：

- `thunk-rs = { version = "0.3", features = ["lib", "win7"], default-features = false }` — Windows 7 兼容导出 thunk
- `winres = "0.1"` — Windows 资源编译

设置 crate 类型、feature 与 release 构建优化：

```toml
[lib]
crate-type = ["cdylib"]

[features]
default = []
logging = ["dep:tracing-subscriber"]

[profile.release]
codegen-units = 1
lto = true
opt-level = "z"
panic = "abort"
strip = "symbols"
```

> `cdylib` 生成 C 兼容动态库（Windows `.dll` / macOS `.dylib`）。不要同时加 `rlib`，否则会影响符号导出和体积。`panic = "abort"` 与 `catch_unwind` 在 Release 下配合使用时，uncaught panic 直接终止进程，已被 FFI 层 `catch_unwind` 拦截处理。

### 2. `src/config.rs` — 配置与常量

```rust
/// 协议版本号，握手时交换，不兼容则拒绝连接
pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug)]
pub struct NetworkConfig {
    /// 压缩阈值（字节），超过此大小自动压缩，默认 512
    pub compression_threshold: u32,
    /// 最大消息大小（payload），防止内存耗尽，默认 256*1024 (256 KiB)
    pub max_message_size: u32,
    /// 心跳间隔（同时控制 Ping/Pong 发送频率和 Raft Heartbeat 发送频率），默认 500ms
    pub heartbeat_interval_ms: u64,
    /// 选举超时范围（最小值），默认 1500ms
    pub election_timeout_min_ms: u64,
    /// 选举超时范围（最大值），默认 3000ms
    pub election_timeout_max_ms: u64,
    /// 存活超时倍数（连续 N 个周期未收到 Pong 回复则判定离线），默认 3
    pub heartbeat_timeout_multiplier: u32,
    /// 重连初始间隔，默认 1000ms
    pub reconnect_initial_ms: u64,
    /// 重连最大间隔，默认 30000ms
    pub reconnect_max_ms: u64,
    /// 发送队列最大长度，默认 128
    pub send_queue_capacity: usize,
    /// 中心化模式下 Leader 是否自动转发消息，默认 true
    pub centralized_auto_forward: bool,
    /// 是否默认启用自动选举，默认 true
    pub auto_election_enabled: bool,
    /// 手动指定 Leader 掉线后的恢复策略，默认 Hold
    pub manual_override_recovery: ManualOverrideRecovery,
    /// 最大同时连接数，防止资源耗尽，默认 64
    pub max_connections: usize,
    /// TCP 握手超时（ms），默认 5000
    pub handshake_timeout_ms: u64,
    /// mDNS 发现端口，默认 15353
    pub mdns_port: u16,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            compression_threshold: 512,
            max_message_size: 256 * 1024, // 256 KiB
            heartbeat_interval_ms: 500,
            election_timeout_min_ms: 1500,
            election_timeout_max_ms: 3000,
            heartbeat_timeout_multiplier: 3,
            reconnect_initial_ms: 1000,
            reconnect_max_ms: 30000,
            send_queue_capacity: 128,
            centralized_auto_forward: true,
            auto_election_enabled: true,
            manual_override_recovery: ManualOverrideRecovery::Hold,
            max_connections: 64,
            handshake_timeout_ms: 5000,
            mdns_port: 15353,
        }
    }
}

impl NetworkConfig {
    /// 验证配置合理性，返回第一个发现的错误。
    /// 约束：heartbeat > 0, election_min > heartbeat, election_min ≤ election_max,
    /// timeout_multiplier > 0, reconnect_initial > 0 且 ≤ reconnect_max, queue > 0
    pub fn validate(&self) -> Result<(), NetworkError> { /* 逐项检查上述约束 */ }
}
```

### 3. `src/types.rs` — 公共类型

定义核心结构体与枚举：

- `PeerId`：字符串型 peer 标识（包装 `String`，实现 `Hash`, `Eq`, `Clone`）
- `PeerStatus` 枚举：`Connected`, `Disconnected`, `Reconnecting`, `Handshaking`
  - 提供 `as_i32()` 方法用于 FFI 回调/外部消费，映射为 0/1/2/3
- `PeerInfo` 结构体：
  - `peer_id: PeerId`
  - `addr: SocketAddr` — 对端的监听地址（IP + listen_port），用于断线重连
  - `status: PeerStatus`
  - `last_seen: Instant` — 最后一次收到 Pong 或其他消息的时间，用于存活判定
  - `rtt_ms: Option<u32>` — 往返延迟（由 Ping/Pong 测量）
  - `connected_at: Instant`
  - `should_reconnect: bool` — 断线后是否自动重连（`DisconnectPeer` 设为 false）
- `MessageTarget` 枚举：`Broadcast`, `ToPeer(PeerId)`, `ToLeader`
- `ForwardTarget` 枚举：`ToPeer(PeerId)`, `Broadcast` — Leader 转发消息时的目标类型

### 4. `src/protocol.rs` — 协议定义

**消息头格式（7 字节）：**

| 字段       | 大小    | 说明                                                                       |
| ---------- | ------- | -------------------------------------------------------------------------- |
| `length`   | 4 bytes | payload 长度（大端），不含 header 的 7 字节                                |
| `msg_type` | 2 bytes | 消息类型（大端）                                                           |
| `flags`    | 1 byte  | bit 0: 已压缩（库内部管理，编码时会清除用户传入该位）；bit 1-7: 用户自定义 |

`total_bytes = 7 (header) + length (payload)`

**msg_type 范围定义：**

| 范围          | 用途           |
| ------------- | -------------- |
| 0x0001–0x00FF | 内部协议消息   |
| 0x0100–0xFFFF | 用户自定义消息 |

**flags 语义：**

- **bit 0 (`0x01`)：压缩标志** — 由库的编码层自动设置/读取。编码函数会先将传入的 `user_flags` 低位清零，再根据压缩情况设置该位；因此 C# 不应手动操作此位。
- **bit 1-7：用户自定义标志** — C# 可自由使用（如标记消息优先级、是否需要回执等），库透传不修改。

**InternalMessage 枚举（完整）：**

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum InternalMessage {
    // === 连接管理 (0x01–0x0F) ===
    Handshake {
        peer_id: String,
        listen_port: u16,
        protocol_version: u16,
        session_id: String,
    },
    HandshakeAck {
        peer_id: String,
        listen_port: u16, // 被动方也回传自己的 listen_port，供后续 PeerListSync 使用
        success: bool,
        error_reason: Option<String>,
    },
    PeerLeave {
        peer_id: String,
    },
    PeerListSync {
        peers: Vec<(String, String)>, // Vec<(peer_id, listen_addr)>
    },

    // === 心跳与存活探测 (0x10–0x1F) ===
    Heartbeat {
        term: u64,
        leader_id: String,
        timestamp_ms: u64,
    },
    HeartbeatResponse {
        term: u64,
        timestamp_ms: u64,
    },
    Ping {
        timestamp_ms: u64,
    },
    Pong {
        timestamp_ms: u64,
    },

    // === 选举 (0x20–0x2F) ===
    RequestVote {
        term: u64,
        candidate_id: String,
    },
    VoteResponse {
        term: u64,
        voter_id: String,
        granted: bool,
    },
    LeaderAssign {
        term: u64,
        leader_id: String,
        assigner_id: String,
    },

    // === 中心化模式消息转发 (0x30–0x3F) ===
    ForwardedUserData {
        from_peer_id: String,
        original_msg_type: u16,
        original_flags: u8,
        payload: Vec<u8>,
    },
}

/// msg_type 常量
pub mod msg_types {
    pub const HANDSHAKE: u16 = 0x0001;
    pub const HANDSHAKE_ACK: u16 = 0x0002;
    pub const PEER_LEAVE: u16 = 0x0003;
    pub const PEER_LIST_SYNC: u16 = 0x0004;
    pub const HEARTBEAT: u16 = 0x0010;
    pub const HEARTBEAT_RESPONSE: u16 = 0x0011;
    pub const PING: u16 = 0x0012;
    pub const PONG: u16 = 0x0013;
    pub const REQUEST_VOTE: u16 = 0x0020;
    pub const VOTE_RESPONSE: u16 = 0x0021;
    pub const LEADER_ASSIGN: u16 = 0x0022;
    pub const FORWARDED_USER_DATA: u16 = 0x0030;
    /// 用户消息起始值
    pub const USER_MESSAGE_START: u16 = 0x0100;
}
```

### 5. `src/error.rs` — 错误处理

**错误码常量定义在此模块**，避免与 ffi.rs 形成循环依赖：

```rust
use std::io;

/// FFI 错误码常量（定义在 error.rs，ffi.rs 直接引用）
pub mod error_codes {
    pub const OK: i32 = 0;
    pub const NOT_INITIALIZED: i32 = -1;
    pub const ALREADY_INITIALIZED: i32 = -2;
    pub const INVALID_ARGUMENT: i32 = -3;
    pub const CONNECTION_FAILED: i32 = -4;
    pub const PEER_NOT_FOUND: i32 = -5;
    pub const NOT_LEADER: i32 = -6;
    pub const SEND_QUEUE_FULL: i32 = -7;
    pub const MESSAGE_TOO_LARGE: i32 = -8;
    pub const SERIALIZATION_ERROR: i32 = -9;
    pub const SESSION_MISMATCH: i32 = -10;
    pub const DUPLICATE_PEER_ID: i32 = -11;
    pub const VERSION_MISMATCH: i32 = -12;
    pub const MAX_CONNECTIONS_REACHED: i32 = -13;
    pub const ALREADY_CONNECTED: i32 = -14;
    pub const INTERNAL_ERROR: i32 = -99;
}

#[derive(Debug)]
pub enum NetworkError {
    NotInitialized,
    AlreadyInitialized,
    InvalidArgument(String),
    Io(io::Error),
    ConnectionFailed(String),
    PeerNotFound(String),
    NotLeader,
    SendQueueFull,
    MessageTooLarge(u32),
    Serialization(postcard::Error),
    SessionMismatch { expected: String, got: String },
    DuplicatePeerId(String),
    VersionMismatch { expected: u16, got: u16 },
    MaxConnectionsReached,
    AlreadyConnected(String),
    HandshakeFailed(String),
    HandshakeTimeout,
    NotImplemented,
    Internal(String),
}

impl NetworkError {
    /// 每个变体映射到对应的 error_codes 常量（如 Io/ConnectionFailed → CONNECTION_FAILED）
    pub fn error_code(&self) -> i32 { /* match self → error_codes::* */ }
}

// 另需实现：Display（每个变体一行人可读描述）、Error、From<io::Error>、From<postcard::Error>
```

---

## Verification

- `cargo check` 通过编译
- 所有类型实现所需的 derive trait（`Serialize`, `Deserialize`, `Debug`, `Clone` 等）
- `NetworkError` 到错误码的映射覆盖所有变体（含 `HandshakeTimeout`、`AlreadyConnected`）
- `error_codes` 定义在 `error.rs` 中，无循环依赖
- `NetworkConfig::default()` 各字段值合理
- `NetworkConfig::validate()` 拒绝非法参数组合
- `HandshakeAck` 包含 `listen_port` 字段
- `flags` 文档明确 bit 0 为库内部使用

## 实现映射（关键常量与 FFI 相关项）

- **NetworkConfigFFI**：位于 `src/ffi.rs` 的 `NetworkConfigFFI` 与本模块的 `NetworkConfig` 通过 `impl From<&NetworkConfigFFI> for NetworkConfig` 映射（字段一一对应，padding 已考虑）。
- **错误码**：`error_codes` 常量定义在 `src/error.rs`，FFI 函数统一返回 `i32` 错误码并可通过 `GetLastErrorCode`/`GetLastErrorMessage` 查询。
- **消息类型边界**：`msg_types::USER_MESSAGE_START == 0x0100` 为用户消息起点，FFI 与发送接口均在入口处验证该边界（见 `src/ffi.rs` 的 `BroadcastMessage` / `SendToPeer` 调用链）。

（此处内容为对实现的精确映射补充，用于后续子计划的对齐与查证。）
