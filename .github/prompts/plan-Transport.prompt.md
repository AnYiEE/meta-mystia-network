# Plan: 传输层

> 隶属于 `plan-MetaMystiaNetwork` 总纲 — 阶段 2：传输层
>
> 依赖：`plan-CoreTypesProtocol`（config、types、protocol、error）

本计划覆盖 TCP 连接管理与消息编解码，是网络通信的核心基础设施。

注意：文档同时列出内部 Rust 方法（snake_case）与 FFI 导出函数（CamelCase），并注明对应关系。例如：内部 `PacketCodec::new(max_message_size)`，FFI 层的 `ConnectToPeer`。

---

## Steps

### 1. `src/messaging.rs` — 消息编解码

**RawPacket 结构体：**

```rust
#[derive(Debug, Clone)]
pub struct RawPacket {
    pub msg_type: u16,
    pub flags: u8,
    pub payload: bytes::Bytes, // 使用 Bytes 以便零拷贝共享
}

impl RawPacket {
    pub fn is_compressed(&self) -> bool {
        self.flags & 0x01 != 0
    }

    pub fn is_internal(&self) -> bool {
        self.msg_type < msg_types::USER_MESSAGE_START
    }
}
```

**功能：**

1. **encode_packet(msg_type, data, user_flags, compression_threshold, max_message_size) -> RawPacket**：
   - **msg_type 验证**：若 `msg_type < USER_MESSAGE_START`（即 `< 0x0100`），返回 `NetworkError::InvalidArgument`（仅用户消息调用此函数；内部消息使用 `encode_internal`）
   - **flags 预处理**：先将 `user_flags` 的低位（压缩标志位）清零——`let clean_flags = user_flags & 0xFE`。后续步骤基于 `clean_flags` 构建返回值。
   - **大小检查**：若 `data.len() as u32 > max_message_size`，立即返回 `NetworkError::MessageTooLarge`
   - 如果 `data.len()` 超过 `compression_threshold`：
     - 调用 `lz4_flex::block::compress_prepend_size` 压缩
     - 若压缩结果比原始数据短，返回 `RawPacket { msg_type, flags: clean_flags | 0x01, payload: Bytes::from(compressed) }`（低位用于标记已压缩）
     - 否则（压缩无利），忽略压缩结果，继续往下返回原始数据
   - 如果 `data.len() <= compression_threshold` 或压缩不生成更小结果：
     - 返回 `RawPacket { msg_type, flags: clean_flags, payload: Bytes::copy_from_slice(data) }`，不设置压缩位

2. **decode_payload(packet: &RawPacket) -> Result<bytes::Bytes, NetworkError>**：
   - 若 `flags & 0x01`，使用 `lz4_flex::block::decompress_size_prepended` 解压
   - 返回解压后的原始数据

3. **内部控制消息序列化**：

   ```rust
   pub fn encode_internal(msg: &InternalMessage, max_message_size: u32) -> Result<RawPacket, NetworkError> {
       let payload = postcard::to_allocvec(msg)?;
       let msg_type = match msg {
           InternalMessage::Handshake { .. } => msg_types::HANDSHAKE,
           InternalMessage::HandshakeAck { .. } => msg_types::HANDSHAKE_ACK,
           // ... 其他类型映射
       };
       Ok(RawPacket { msg_type, flags: 0, payload })
   }

   pub fn decode_internal(packet: &RawPacket) -> Result<InternalMessage, NetworkError> {
       postcard::from_bytes(&packet.payload).map_err(NetworkError::Serialization)
   }
   ```

4. **消息大小验证**：
   - 编码前检查 payload 是否超过 `max_message_size`（来自 `NetworkConfig` 字段）
   - 超过则返回 `NetworkError::MessageTooLarge`

### 2. `src/transport.rs` — TCP 传输层

**核心结构：**

- `PeerConnection`：管理一个到远程 peer 的 TCP 连接
  - 使用 `tokio_util::codec::Framed<TcpStream, PacketCodec>` 处理粘包
  - 持有 `mpsc::Sender<RawPacket>` 用于发送队列
  - 持有 `CancellationToken` 用于优雅关闭
- `TransportManager`：管理所有连接 + TCP listener
  - `local_peer_id: PeerId` — 本地 Peer ID
  - `session_id: String` — 用于握手验证
  - `config: NetworkConfig`
  - `connections: Arc<RwLock<HashMap<PeerId, PeerConnection>>>`
  - `listener_addr: SocketAddr` — 本地监听地址
  - `shutdown_token: CancellationToken` — 全局关闭信号

**构造函数返回值**（解耦 IO 与逻辑，提升可测试性）：

```rust
impl TransportManager {
    pub async fn new(
        local_peer_id: PeerId,
        session_id: String,
        config: NetworkConfig,
        shutdown_token: CancellationToken,
    ) -> Result<(Arc<Self>, mpsc::Receiver<(PeerId, RawPacket)>), NetworkError> {
        let (incoming_tx, incoming_rx) = mpsc::channel(256); // 汇总所有 peer 的入站消息，容量独立于 per-peer send_queue
        // ... 绑定 TCP listener、启动 accept 循环 ...
        Ok((manager, incoming_rx))
    }
}
```

> `incoming_rx` 由调用方（`NetworkState::new`）传给 `spawn_message_handler`，而非藏在 `Arc<TransportManager>` 中。这样 `TransportManager` 自身不持有 `Receiver`（不可 Clone），消息处理逻辑与 Transport 完全解耦，便于分别测试。

**PacketCodec（基于 `tokio-util` 的 Encoder/Decoder 框架，代码中以 `tokio_util` 引用）：**

此处使用 `tokio-util` 提供的 `Framed`/codec 框架（代码中以 `tokio_util::codec::Framed` 引用）。项目实现了自定义的 `PacketCodec`（替代通用的 `LengthDelimitedCodec`），以满足 7 字节头部的自定义协议。

```rust
pub struct PacketCodec {
    max_message_size: u32,
}

impl Decoder for PacketCodec {
    type Item = RawPacket;
    type Error = NetworkError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 7 { return Ok(None); }

        let length = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);
        if length > self.max_message_size {
            return Err(NetworkError::MessageTooLarge(length));
        }

        let total = 7 + length as usize;
        if src.len() < total {
            src.reserve(total - src.len());
            return Ok(None);
        }

        let header = src.split_to(7);
        let payload = src.split_to(length as usize);

        Ok(Some(RawPacket {
            msg_type: u16::from_be_bytes([header[4], header[5]]),
            flags: header[6],
            // returns an owned `Bytes` buffer
            payload: payload.freeze(),
        }))
    }
}

impl Encoder<RawPacket> for PacketCodec {
    type Error = NetworkError;

    fn encode(&mut self, item: RawPacket, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let length = item.payload.len() as u32;
        dst.reserve(7 + item.payload.len());
        dst.put_u32(length);
        dst.put_u16(item.msg_type);
        dst.put_u8(item.flags);
        dst.put_slice(&item.payload);
        Ok(())
    }
}
```

**功能实现：**

1. **TCP Listener**：
   - 绑定 `0.0.0.0:0`（系统分配端口）
   - `accept` 循环，每个新连接 spawn 到 `handle_incoming_connection`
   - 连接数达到 `config.max_connections` 时拒绝新连接

2. **TCP Keep-alive**：
   - 使用 `socket2` crate 设置 TCP 层 Keep-alive
   - `keepalive_time = 60s`，`keepalive_interval = 10s`
   - `keepalive_retries = 3`（**仅 macOS/Linux**；Windows 无 `TCP_KEEPCNT`，跳过此设置而非报错）

3. **连接握手（含超时）**：
   - 新 TCP 连接建立后，启动 `config.handshake_timeout_ms`（5s）计时器
   - 主动方发送 `Handshake { peer_id, listen_port, protocol_version, session_id }`
   - 被动方验证：
     - `protocol_version` 必须兼容
     - `session_id` 必须匹配（房间隔离）
     - `peer_id` 不能与已有连接冲突
   - 验证通过返回 `HandshakeAck { peer_id, listen_port, success: true }`，随后发送 `PeerListSync`
   - 验证失败返回 `HandshakeAck { success: false, error_reason }` 并关闭连接
   - **AlreadyConnected 处理**：若 `peer_id` 已存在于连接表中（`has_peer()`），`receive_handshake` 发送 `HandshakeAck(success=true, error_reason="already_connected")` 并返回 `NetworkError::AlreadyConnected`（非 fatal），被动方保留已有连接不受影响。`connect_to` 收到此类 ack 后也返回 `AlreadyConnected`。此外，`handle_incoming_connection` 和 `connect_to` 均在调用 `register_connection` 后捕获 `DuplicatePeerId`（TOCTOU 保护——`receive_handshake` 的 `has_peer()` 检查与 `register_connection` 注册之间存在时间窗口），将其静默处理为保留已有连接。这使得重复连接成为非致命的预期结果。
   - **超时处理**：5s 内未完成握手（未收到 Handshake 或 HandshakeAck），关闭连接并返回 `NetworkError::HandshakeTimeout`（在 FFI 层映射为 `error_codes::CONNECTION_FAILED`）。
   - **成功后通知**：握手成功后，Transport 将收到的 Handshake（被动方）或 HandshakeAck（主动方）转发至 `incoming_rx`，消息处理器据此调用 `membership.add_peer()` 并触发 `PeerJoined` 事件

4. **PeerListSync 去重**：
   - 收到 `PeerListSync` 后，对每个未知 peer 发起连接
   - **使用与 mDNS 相同的去重规则**：仅当 `local_peer_id < target_peer_id`（字典序）时主动连接，避免双向同时连接

5. **自动重连**：
   - 检测到连接断开后，检查 `PeerInfo.should_reconnect`：
     - `true`（默认，意外断线）：`tokio::spawn` 重连任务
     - `false`（`DisconnectPeer` 调用后）：不重连
   - 指数退避：初始 `reconnect_initial_ms`，倍增至 `reconnect_max_ms`
   - 使用 `CancellationToken` 在 ShutdownNetwork 时中止重连

6. **消息发送接口**：
   - `send_to_peer(peer_id, packet) -> Result<(), NetworkError>`
   - `broadcast(packet, exclude: Option<&[PeerId]>) -> Vec<(PeerId, NetworkError)>` 返回失败列表
   - 发送队列满时返回 `NetworkError::SendQueueFull`
7. **其他公开辅助方法**（供测试或上层逻辑使用）：
   - `get_connected_peer_ids() -> Vec<PeerId>`
   - `has_peer(peer_id) -> bool`
   - `set_should_reconnect(peer_id, bool)`
   - `update_peer_status(peer_id, PeerStatus)`
   - `update_last_seen(peer_id)` / `update_rtt(peer_id, rtt_ms)`
   - `get_peer_rtt(peer_id) -> Option<u32>` / `get_peer_status(peer_id) -> Option<PeerStatus>`
   - `get_peer_addr(peer_id) -> Option<SocketAddr>`
     这组方法主要用于 `lib.rs` 的消息处理和测试验证。

8. **DisconnectPeer**：
   - 向目标 peer 发送 `PeerLeave` 消息（最尽力，发送失败时只记录 debug 日志，不报错）
   - 设置 `conn.info.should_reconnect = false`
   - 取消 per-peer `CancellationToken`（停止读/写 task）
   - 从 connections map 中移除
   - 注意：membership 的移除由调用方负责（FFI 的 `DisconnectPeer` 另行调用 `membership.remove_peer()`）

9. **优雅关闭（顺序关键）**：
   1. 向所有连接发送 `PeerLeave` 消息（此时 write task 仍在运行）
   2. 等待发送队列清空（最多 1s）
   3. 触发 `shutdown_token.cancel()`（通知所有 task 退出）
   4. 设置所有 peer 的 `should_reconnect = false`，清空 connections map

**线程模型**：每个连接 spawn 两个 tokio task：

- Read task：循环读取并解码消息，通过 channel 发送给消息处理器
- Write task：从 `mpsc::Receiver<RawPacket>` 接收并发送消息

---

## Verification

- `PacketCodec` 单元测试：粘包处理（分包、半包、多包粘连）
- `Encoder` 实现的输出与 `Decoder` 往返一致
- `encode_packet` 拒绝 `msg_type < USER_MESSAGE_START`
- `encode_packet` / `decode_payload` 单元测试：各种大小 + 压缩阈值边界 + user_flags 保留
- `encode_internal` / `decode_internal` 单元测试：InternalMessage 所有变体序列化/反序列化往返
- 消息大小验证：超过 `max_message_size`（`NetworkConfig` 字段）时返回正确错误
- 2 节点 loopback TCP 连接 + 握手成功集成测试
- 握手版本不匹配/session 不匹配/重复 peer_id 时返回失败并发送含 reason 的 `HandshakeAck`
- 握手超时测试：一方不发 Handshake，5s 后连接关闭
- 超过 `config.max_connections` 时拒绝新连接
- `DisconnectPeer` 后不触发自动重连
- PeerListSync 触发的连接遵循字典序去重规则
- **TCP Keep-alive 在 Windows 和 macOS 上均不报错**（Windows 跳过 retries 设置）

## 实现映射（关键函数/方法）

- FFI → Transport：`ConnectToPeer` 调用 `transport.connect_to(addr)`（在 `src/ffi.rs` 中通过 tokio runtime spawn 异步执行）；`DisconnectPeer` 调用 `TransportManager::disconnect_peer`。
- Transport 暴露的内部方法：`TransportManager::new(...) -> (Arc<Self>, incoming_rx)`, `connect_to(&self, addr: &str)`, `register_connection`, `send_to_peer(&self, peer_id, packet)`, `broadcast(&self, packet, exclude)`, `drain_send_queues(timeout)`, `shutdown()`。
  另外还有若干辅助查询/控制方法用于上层逻辑与测试：
  `get_connected_peer_ids()`, `has_peer()`, `set_should_reconnect()`, `update_peer_status()`, `update_last_seen()` / `update_rtt()`, `get_peer_rtt()`, `get_peer_status()`, `get_peer_addr()`。
- PacketCodec 行为：`Decoder` 在成功切出 header 与 payload 后使用 `payload.freeze()` 返回 `Bytes`，`Encoder` 在写入前调用 `dst.reserve(7 + payload.len())`；这与 `messaging.rs` 中 `RawPacket` 的零拷贝设计一致。
- 握手流程与超时：`config.handshake_timeout_ms` 在 `NetworkConfig` 中定义（默认 5000ms），`receive_handshake`/`receive_handshake_ack` 在 `transport.rs` 中实现；失败返回 `HandshakeTimeout`（FFI 映射到 `CONNECTION_FAILED`）或 `HandshakeFailed`。

（将此映射作为 Transport 计划的权威引用，便于与 FFI/上层逻辑一一核对。）
