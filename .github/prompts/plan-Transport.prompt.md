# Plan: 传输层

> 隶属于 `plan-MetaMystiaNetwork` 总纲 — 阶段 2：传输层
>
> 依赖：`plan-CoreTypesProtocol`（config、types、protocol、error）

本计划覆盖 TCP 连接管理和消息编解码，是网络通信的核心基础设施。

---

## Steps

### 1. `src/messaging.rs` — 消息编解码

**RawPacket 结构体：**

```rust
#[derive(Debug, Clone)]
pub struct RawPacket {
    pub msg_type: u16,
    pub flags: u8,
    pub payload: Vec<u8>,
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

1. **encode_packet(msg_type, data, user_flags, compression_threshold) -> RawPacket**：
   - **msg_type 验证**：若 `msg_type < USER_MESSAGE_START`（即 `< 0x0100`），返回 `NetworkError::InvalidArgument`（仅用户消息调用此函数；内部消息使用 `encode_internal`）
   - 若 `data.len() > threshold`，使用 `lz4_flex::block::compress_prepend_size` 压缩
   - 压缩后设置 `flags = user_flags | 0x01`（合并用户标志与压缩标志）
   - 若压缩后反而更大，保留原始数据，`flags = user_flags`（不设压缩位）
   - 若 `data.len() <= threshold`，`flags = user_flags`

2. **decode_payload(packet: &RawPacket) -> Result<Vec<u8>, NetworkError>**：
   - 若 `flags & 0x01`，使用 `lz4_flex::block::decompress_size_prepended` 解压
   - 返回解压后的原始数据

3. **内部控制消息序列化**：

   ```rust
   pub fn encode_internal(msg: &InternalMessage) -> Result<RawPacket, NetworkError> {
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
   - 编码前检查 payload 是否超过 `MAX_MESSAGE_SIZE`
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
    ) -> Result<(Self, mpsc::Receiver<(PeerId, RawPacket)>), NetworkError> {
        let (incoming_tx, incoming_rx) = mpsc::channel(256); // 汇总所有 peer 的入站消息，容量独立于 per-peer send_queue
        // ... 绑定 TCP listener、启动 accept 循环 ...
        Ok((manager, incoming_rx))
    }
}
```

> `incoming_rx` 由调用方（`NetworkState::new`）传给 `spawn_message_handler`，而非藏在 `Arc<TransportManager>` 中。这样 `TransportManager` 自身不持有 `Receiver`（不可 Clone），消息处理逻辑与 Transport 完全解耦，便于分别测试。

**PacketCodec（基于 tokio_util Encoder/Decoder）：**

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
        if src.len() < total { return Ok(None); }

        let header = src.split_to(7);
        let payload = src.split_to(length as usize);

        Ok(Some(RawPacket {
            msg_type: u16::from_be_bytes([header[4], header[5]]),
            flags: header[6],
            payload: payload.to_vec(),
        }))
    }
}

impl Encoder<RawPacket> for PacketCodec {
    type Error = NetworkError;

    fn encode(&mut self, item: RawPacket, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let length = item.payload.len() as u32;
        dst.reserve(7 + item.payload.len());
        dst.put_u32(length);           // 4 bytes: payload length (big-endian)
        dst.put_u16(item.msg_type);    // 2 bytes: msg_type (big-endian)
        dst.put_u8(item.flags);        // 1 byte:  flags
        dst.put_slice(&item.payload);  // N bytes: payload
        Ok(())
    }
}
```

**功能实现：**

1. **TCP Listener**：
   - 绑定 `0.0.0.0:0`（系统分配端口）
   - `accept` 循环，每个新连接 spawn 到 `handle_incoming_connection`
   - 连接数达到 `MAX_CONNECTIONS` 时拒绝新连接

2. **TCP Keep-alive**：
   - 使用 `socket2` crate 设置 TCP 层 Keep-alive
   - `keepalive_time = 60s`，`keepalive_interval = 10s`
   - `keepalive_retries = 3`（**仅 macOS/Linux**；Windows 无 `TCP_KEEPCNT`，跳过此设置而非报错）

3. **连接握手（含超时）**：
   - 新 TCP 连接建立后，启动 `HANDSHAKE_TIMEOUT_MS`（5s）计时器
   - 主动方发送 `Handshake { peer_id, listen_port, protocol_version, session_id }`
   - 被动方验证：
     - `protocol_version` 必须兼容
     - `session_id` 必须匹配（房间隔离）
     - `peer_id` 不能与已有连接冲突
   - 验证通过返回 `HandshakeAck { peer_id, listen_port, success: true }`，随后发送 `PeerListSync`
   - 验证失败返回 `HandshakeAck { success: false, error_reason }` 并关闭连接
   - **超时处理**：5s 内未完成握手（未收到 Handshake 或 HandshakeAck），关闭连接并返回 `NetworkError::HandshakeTimeout`
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

7. **DisconnectPeer**：
   - 发送 `PeerLeave` 消息给目标 peer
   - 设置 `PeerInfo.should_reconnect = false`
   - 取消该 peer 的重连 task（通过 per-peer `CancellationToken`）
   - 关闭 TCP 连接
   - 从 membership 中移除

8. **优雅关闭（顺序关键）**：
   1. 向所有连接发送 `PeerLeave` 消息（此时 write task 仍在运行）
   2. 等待发送队列清空（最多 1s）
   3. 触发 `shutdown_token.cancel()`（通知所有 task 退出）
   4. 关闭所有 TCP 连接

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
- 消息大小验证：超过 `MAX_MESSAGE_SIZE` 时返回正确错误
- 2 节点 loopback TCP 连接 + 握手成功集成测试
- 握手超时测试：一方不发 Handshake，5s 后连接关闭
- `DisconnectPeer` 后不触发自动重连
- PeerListSync 触发的连接遵循字典序去重规则
- **TCP Keep-alive 在 Windows 和 macOS 上均不报错**（Windows 跳过 retries 设置）
