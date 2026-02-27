# Plan: FFI 回调与集成

> 隶属于 `plan-MetaMystiaNetwork` 总纲 — 阶段 4：接口层
>
> 依赖：所有前序子计划。测试见 `plan-Tests.prompt.md`。

本计划覆盖 `callback.rs`（回调管理）、`ffi.rs`（C ABI 接口）、`lib.rs`（全局状态）及日志系统。

注意：文档同时参考内部 Rust API（snake_case）与导出到 C# 的 FFI 名称（CamelCase）。引用 FFI 时使用 CamelCase（例如 `InitializeNetwork`）。

---

## Steps

### 1. `src/callback.rs` — 回调管理

**核心结构：**

```rust
pub struct CallbackManager {
    receive_callback: Mutex<Option<ReceiveCallback>>,
    leader_changed_callback: Mutex<Option<LeaderChangedCallback>>,
    peer_status_callback: Mutex<Option<PeerStatusCallback>>,
    connection_result_callback: Arc<Mutex<Option<ConnectionResultCallback>>>,
    event_tx: Mutex<Option<tokio::sync::mpsc::Sender<CallbackEvent>>>,
    callback_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    shutdown: Arc<AtomicBool>,
}
```

**回调签名**（所有布尔用 `u8`）：

| 回调                       | 签名                                                                                                 |
| -------------------------- | ---------------------------------------------------------------------------------------------------- |
| `ReceiveCallback`          | `(peer_id: *const c_char, data: *const u8, length: i32, msg_type: u16, flags: u8)`                   |
| `LeaderChangedCallback`    | `(leader_peer_id: *const c_char)`                                                                    |
| `PeerStatusCallback`       | `(peer_id: *const c_char, status: i32)` — 0=Connected, 1=Disconnected, 2=Reconnecting, 3=Handshaking |
| `ConnectionResultCallback` | `(addr: *const c_char, success: u8, error_code: i32)`                                                |

**设计要点：**

1. 事件入队 `tokio::sync::mpsc`（生产方在 tokio task），专用 `std::thread` 用 `blocking_recv()` 消费，顺序调用 C# 回调
2. `Register*Callback(null)` 等同于注销
3. 回调前将 String 转 `CString`，回调结束后 Rust 释放。C# 必须在回调内同步拷贝字符串
4. `leader_id` 为 None 时传空字符串 `""`（非 null）
5. 队列容量 1024，溢出时丢弃**最新**事件（`try_send` 语义，缓慢消费方不会污染旧事件）
6. 关闭时：`drain_and_shutdown()` 设置 `shutdown=true` 并 drop `event_tx`（Sender drop 致 `blocking_recv()` 返回 `None`，线程自然退出）；`join_thread()` 直接调用 `handle.join()` 等待线程结束（不用 `spawn_blocking`）
7. **防重入**：调用回调前先读函数指针到局部变量再释放 Mutex。C# 回调内**禁止调用任何 FFI 函数**，需缓存到 C# 侧队列由游戏主线程处理

### 2. `src/ffi.rs` — C ABI 接口层

#### 全局状态

```rust
static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);
static NETWORK: Mutex<Option<NetworkState>> = Mutex::new(None);
static LAST_ERROR: Mutex<Option<(i32, String)>> = Mutex::new(None);
static LAST_RETURNED_STRING: Mutex<Option<CString>> = Mutex::new(None);
```

均使用 `parking_lot::Mutex`（支持 `UnwindSafe`，`catch_unwind` 内可安全访问）。`error_codes` 从 `error.rs` 导入。

#### Panic 保护

每个 FFI 函数用 `catch_unwind` 包裹。三种返回类型的模板：

- **返回 `i32`**：panic 时 `set_error(INTERNAL_ERROR, "panic in Xxx")` 并返回 `INTERNAL_ERROR`
- **返回 `u8`**：panic 时返回 `0`
- **返回 `*const c_char`**：panic 时返回 `null`

#### 类型规则

- Rust `extern "C"` 使用 **Cdecl** 调用约定。C# 侧每个 `DllImport` 必须标注 `CallingConvention = CallingConvention.Cdecl`（P/Invoke 在 Windows 默认 StdCall，不匹配会栈损坏）
- 所有 FFI 边界布尔语义统一 `u8`(0/1)，C# 侧声明 `byte`。原因：Rust `bool` = 1B，C# BOOL = 4B，IL2CPP 下 MarshalAs 不可靠

#### FFI 接口列表

| 函数                          | 签名                                                                                                          | 说明                                                                                                           |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| `InitializeNetwork`           | `(peer_id: *const c_char, session_id: *const c_char) -> i32`                                                  | 默认配置初始化                                                                                                 |
| `InitializeNetworkWithConfig` | `(..., config: *const NetworkConfigFFI) -> i32`                                                               | 自定义配置（含 validate）                                                                                      |
| `ShutdownNetwork`             | `() -> i32`                                                                                                   | 优雅关闭                                                                                                       |
| `IsNetworkInitialized`        | `() -> u8`                                                                                                    |                                                                                                                |
| `GetLastErrorCode`            | `() -> i32`                                                                                                   |                                                                                                                |
| `GetLastErrorMessage`         | `() -> *const c_char`                                                                                         |                                                                                                                |
| `ConnectToPeer`               | `(addr: *const c_char) -> i32`                                                                                | 异步，结果通过回调；握手超时/失败会以 `ConnectionFailed` 形式返回；已连接的 peer 返回 `ALREADY_CONNECTED(-14)` |
| `DisconnectPeer`              | `(peer_id: *const c_char) -> i32`                                                                             | 不触发自动重连                                                                                                 |
| `GetLocalAddr`                | `() -> *const c_char`                                                                                         |                                                                                                                |
| `GetLocalPeerId`              | `() -> *const c_char`                                                                                         |                                                                                                                |
| `GetSessionId`                | `() -> *const c_char`                                                                                         |                                                                                                                |
| `GetPeerCount`                | `() -> i32`                                                                                                   | Connected 数量                                                                                                 |
| `GetPeerList`                 | `() -> *const c_char`                                                                                         | `\n` 分隔                                                                                                      |
| `GetPeerRTT`                  | `(peer_id: *const c_char) -> i32`                                                                             | -1=未知                                                                                                        |
| `GetPeerStatus`               | `(peer_id: *const c_char) -> i32`                                                                             | 0/1/2/3/-1                                                                                                     |
| `SetLeader`                   | `(peer_id: *const c_char) -> i32`                                                                             |                                                                                                                |
| `EnableAutoLeaderElection`    | `(enable: u8) -> i32`                                                                                         |                                                                                                                |
| `GetCurrentLeader`            | `() -> *const c_char`                                                                                         | 空串=无                                                                                                        |
| `IsLeader`                    | `() -> u8`                                                                                                    |                                                                                                                |
| `SetCentralizedMode`          | `(enable: u8) -> i32`                                                                                         |                                                                                                                |
| `IsCentralizedMode`           | `() -> u8`                                                                                                    |                                                                                                                |
| `SetCentralizedAutoForward`   | `(enable: u8) -> i32`                                                                                         |                                                                                                                |
| `IsCentralizedAutoForward`    | `() -> u8`                                                                                                    |                                                                                                                |
| `SetCompressionThreshold`     | `(threshold: u32) -> i32`                                                                                     |                                                                                                                |
| `BroadcastMessage`            | `(data: *const u8, length: i32, msg_type: u16, flags: u8) -> i32`                                             | flags bit 0 由库管理                                                                                           |
| `SendToPeer`                  | `(target: *const c_char, data: *const u8, length: i32, msg_type: u16, flags: u8) -> i32`                      |                                                                                                                |
| `SendToLeader`                | `(data: *const u8, length: i32, msg_type: u16, flags: u8) -> i32`                                             |                                                                                                                |
| `SendFromLeader`              | `(data: *const u8, length: i32, msg_type: u16, flags: u8) -> i32`                                             |                                                                                                                |
| `ForwardMessage`              | `(from: *const c_char, target: *const c_char, data: *const u8, length: i32, msg_type: u16, flags: u8) -> i32` | target=null 广播                                                                                               |
| `Register*Callback`           | `(callback: FnPtr) -> i32`                                                                                    | null=注销，共 4 个                                                                                             |
| `EnableLogging`               | `(enable: u8) -> i32`                                                                                         | 仅首次生效                                                                                                     |

#### NetworkConfigFFI

字段按大小降序排列，`#[repr(C)]` 布局（总大小 80 字节）：

```rust
#[repr(C)]
pub struct NetworkConfigFFI {
    pub heartbeat_interval_ms: u64,        // offset  0
    pub election_timeout_min_ms: u64,      // offset  8
    pub election_timeout_max_ms: u64,      // offset 16
    pub heartbeat_timeout_multiplier: u32, // offset 24
    // [4 B implicit padding]              // offset 28
    pub reconnect_initial_ms: u64,         // offset 32
    pub reconnect_max_ms: u64,             // offset 40
    pub compression_threshold: u32,        // offset 48
    pub send_queue_capacity: u32,          // offset 52
    pub max_connections: u32,              // offset 56
    pub max_message_size: u32,             // offset 60
    pub centralized_auto_forward: u8,      // offset 64
    pub auto_election_enabled: u8,         // offset 65
    pub mdns_port: u16,                    // offset 66
    pub manual_override_recovery: u8,      // offset 68 — 0=Hold, 1=AutoElect
    pub _padding: [u8; 3],                 // offset 69 — 对齐填充
    pub handshake_timeout_ms: u64,         // offset 72
}   // sizeof = 80
```

C# 侧用 `[StructLayout(LayoutKind.Sequential)]`，`u64→ulong`，`u32→uint`，`u8→byte`，padding 用 3 个 `private byte`（`_padding1`, `_padding2`, `_padding3`）。

`InitializeNetworkWithConfig` 先转换为 `NetworkConfig`，再调 `config.validate()` 验证参数。

#### 字符串返回

```rust
fn return_string(s: String) -> *const c_char {
    let cstring = CString::new(s).unwrap_or_default();
    let ptr = cstring.as_ptr();
    *LAST_RETURNED_STRING.lock() = Some(cstring);
    ptr
}
```

指针仅在下一次返回字符串的 FFI 调用前有效。C# 用 `Marshal.PtrToStringAnsi()` 同步拷贝。**C# 侧不应并发调用返回字符串的函数**。

### 3. `src/lib.rs` — 全局状态聚合

**NetworkState** 持有所有子系统的 `Arc` 引用 + `CancellationToken`。

**构造顺序**（`NetworkState::new`）：

1. 创建 `CancellationToken`
2. `TransportManager::new()` → 解构得到 `(transport, incoming_rx)`
3. `MembershipManager::new()`
4. `LeaderElection::new(local_peer_id, auto_election_enabled, manual_override_recovery, leader_change_tx)`
5. `SessionRouter::new()` → 持有 transport + leader_election
6. `CallbackManager::new()` → 订阅 leader_change + membership 事件
7. `DiscoveryManager::new()` → 持有 transport + membership
8. `spawn_message_handler(incoming_rx, ...)` — 启动消息分发循环

**关闭顺序**（`NetworkState::shutdown`）：

1. `transport.broadcast_peer_leave()` — write task 仍在运行
2. `transport.drain_send_queues(1s)` — 等待发送完成
3. `shutdown_token.cancel()` — 通知所有 task 退出
4. `discovery.shutdown()` — 注销 mDNS
5. `callback.drain_and_shutdown()` — 清空回调队列、结束回调线程事件循环
6. `transport.shutdown()` — 关闭 TCP 连接
7. `callback.join_thread()` — 等待回调线程真正退出（`handle.join()`）

**ShutdownNetwork FFI 桥接**：

1. `NETWORK.lock().take()` 取出 state
2. `RUNTIME.lock().as_ref().block_on(state.shutdown())` — Runtime 仍存活时执行
3. `RUNTIME.lock().take().shutdown_timeout(5s)` — 销毁 Runtime

**消息分发表**（`spawn_message_handler`）：

| msg_type             | 处理方               | 动作                                                  |
| -------------------- | -------------------- | ----------------------------------------------------- |
| Handshake（转发）    | handler → membership | Transport 验证后转发；调 `add_peer` + 触发 PeerJoined |
| HandshakeAck（转发） | handler → membership | 主动方收到 success=true 时同上                        |
| PeerLeave            | membership           | 移除 peer，触发 PeerLeft                              |
| PeerListSync         | handler              | 向未知 peer 发起连接（字典序去重）                    |
| Ping                 | handler              | 回复 Pong                                             |
| Pong                 | membership           | 更新 last_seen + RTT                                  |
| Heartbeat            | leader_election      | 重置选举超时                                          |
| HeartbeatResponse    | leader_election      | 确认 Follower 存活                                    |
| RequestVote          | leader_election      | 返回 VoteResponse                                     |
| VoteResponse         | leader_election      | 统计票数                                              |
| LeaderAssign         | leader_election      | 手动覆盖                                              |
| ForwardedUserData    | session_router       | 转发或触发 ReceiveCallback                            |
| ≥ USER_MESSAGE_START | callback             | 触发 ReceiveCallback                                  |

**周期性 task**（各自独立 `tokio::spawn`，共 3 个）：

1. **Ping + 存活检测**：每 `heartbeat_interval_ms` 向所有 Connected peer 发 Ping，并检查 `last_seen` 超时，标记 Disconnected（两个职责合并在同一 task 中）
2. **Raft Heartbeat**：Leader 每 `heartbeat_interval_ms` 广播 Heartbeat
3. **选举超时**：随机 `election_timeout` 到期后发起选举

### 4. 日志

- 使用 `tracing` crate，`logging` feature 控制 `tracing-subscriber` 可选依赖
- `EnableLogging(1)` 首次调用初始化 `tracing_subscriber::fmt().with_ansi(false)`（禁用 ANSI 色彩码），后续调用无操作
- 无 `logging` feature 时静默忽略

| 级别    | 内容                                           |
| ------- | ---------------------------------------------- |
| `error` | 连接失败、序列化错误、panic 边界               |
| `warn`  | 心跳超时、握手拒绝、队列溢出、broadcast Lagged |
| `info`  | 连接建立/断开、Leader 变更、mDNS 注册/发现     |
| `debug` | 选举状态转换、term 变化、路由决策              |
| `trace` | 每条消息收发、压缩比、RTT 值                   |

---

## Verification

- 所有 FFI 函数有 `catch_unwind`，无 `bool` 跨边界
- `NetworkConfigFFI` 大小为 80 字节（含 1 处隐式 padding（offset 28）+ 1 处显式 padding（offset 69，`_padding: [u8; 3]`））
- Shutdown 顺序：PeerLeave → drain → cancel → discovery → drain_callback → transport → join_callback_thread → destroy Runtime
- Shutdown 后可再次 Initialize
- `error_codes` 从 `error.rs` 导入
- Windows/macOS 均编译通过

## 精确实现映射（FFI → 内部行为摘要）

- `InitializeNetwork` / `InitializeNetworkWithConfig` → 创建 `tokio::runtime::Runtime`、调用 `NetworkState::new(peer_id, session_id, config)`，并把 `NetworkState` 存入全局单例 `NETWORK`（见 `src/ffi.rs::initialize_network_inner`）。
- `ShutdownNetwork` → 取出 `NetworkState` 并在 `RUNTIME` 上 `block_on(state.shutdown())`，随后 `RUNTIME.shutdown_timeout(5s)` 清理 Runtime。
- `ConnectToPeer(addr)` → 在 FFI 中 spawn 异步任务调用 `transport.connect_to(&addr)`；完成与否通过 `ConnectionResultCallback` 回调通知 C#（见 `callback.rs`）。
- `DisconnectPeer(peer_id)` → 调用 `TransportManager::disconnect_peer`（该方法发送 PeerLeave、设 `should_reconnect=false`、取消 per-peer token、从连接表移除）；**之后** FFI 层再调用 `membership.remove_peer(peer_id)`。`disconnect_peer` 本身不操作 `MembershipManager`，由 FFI 层协调两步调用。
- 消息发送链：`BroadcastMessage`/`SendToPeer`/`SendToLeader` 在 FFI 层做参数校验（`msg_type`、长度、清除 flags bit0），然后调用 `session_router.route_message(...)`；`SendFromLeader` 直接调用 `session_router.send_from_leader(...)`；`ForwardMessage` 调用 `session_router.forward_message(...)`。
- Leader 操作：`SetLeader` 调用 `leader_election.set_leader` 并构建 `InternalMessage::LeaderAssign` 由 `transport.broadcast` 广播；`EnableAutoLeaderElection` 控制 `leader_election.enable_auto_election`。
- 回调注册：`Register*Callback` 将函数指针保存到 `CallbackManager`，事件由专用阻塞线程调用 C# 回调（回调内禁止调用 FFI）。

（本节作为 FFI 与内部实现的一览表，便于审阅者直接定位实现代码以验证行为。）
