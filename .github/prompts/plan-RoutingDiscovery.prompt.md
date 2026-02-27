# Plan: 消息路由与节点发现

> 隶属于 `plan-MetaMystiaNetwork` 总纲 — 阶段 3b：业务层
>
> 依赖：`plan-CoreTypesProtocol`、`plan-Transport`、`plan-MembershipLeader`

本计划覆盖消息路由（含中心化模式）与 mDNS 局域网发现。

---

## Steps

### 1. `src/session_router.rs` — 消息路由

**核心结构：**

```rust
pub struct SessionRouter {
    local_peer_id: PeerId,
    transport: Arc<TransportManager>,
    leader_election: Arc<LeaderElection>,
    centralized_mode: AtomicBool,       // 初始 false（P2P 模式），通过 SetCentralizedMode 切换
    centralized_auto_forward: AtomicBool, // 初始跟随 config.centralized_auto_forward（默认 true）
    compression_threshold: AtomicU32,    // 可在运行时调整
}
```

**功能：**

1. **发送前 msg_type 验证**：
   - 所有用户消息发送函数（`route_message`、`forward_message`）在入口处检查 `msg_type >= USER_MESSAGE_START`（0x0100）
   - 若违反，返回 `NetworkError::InvalidArgument("msg_type must be >= 0x0100")`
   - 防止 C# 误用内部协议消息类型
   - 所有发送路径亦会先对传入的 `flags` 做 `flags & 0xFE` 清除压缩位，后续由 `encode_packet` 根据阈值/压缩结果设置该位。

2. **路由逻辑**：

   | Target       | 非中心化模式      | 中心化模式（非 Leader）     | 中心化模式（Leader）      |
   | ------------ | ----------------- | --------------------------- | ------------------------- |
   | `Broadcast`  | 直接发给所有 peer | 发给 Leader，由 Leader 转发 | 直接发给所有 peer         |
   | `ToPeer(id)` | 直接发给目标 peer | 直接发给目标 peer           | 直接发给目标 peer         |
   | `ToLeader`   | 发给 Leader       | 发给 Leader                 | 本地处理（自己是 Leader） |

3. **中心化模式**（`centralized_mode: bool`）：
   - **启用时**：
     - 非 Leader 调用 `BroadcastMessage` → 消息包装为 `ForwardedUserData` 发给 Leader
     - Leader 收到 `ForwardedUserData` 后，行为取决于 `centralized_auto_forward`：
       - **auto_forward = true（默认）**：Leader 自动将消息广播给除发送者以外的所有 peer，同时触发本地 `ReceiveCallback`
       - **auto_forward = false**：Leader 仅触发 `ReceiveCallback`，由 C# 代码决定是否调用 `ForwardMessage` 转发
     - 可通过 FFI `SetCentralizedAutoForward(enable: u8)` 动态切换
   - **禁用时**：普通 P2P 直连互传

4. **flags 处理**：
   - 用户消息的 `flags` 参数中，**bit 0（0x01）由库管理**（压缩标志），用户不应设置
   - 发送时库自动清除用户传入的 bit 0，压缩逻辑由 `encode_packet` 内部决定
   - bit 1-7 由用户自定义，库透传

5. **ForwardMessage（Leader 专用）**：

   ```rust
   pub fn forward_message(
       &self,
       from_peer_id: &PeerId,
       target: ForwardTarget,
       msg_type: u16,
       flags: u8,
       payload: &[u8],
   ) -> Result<(), NetworkError> {
       if msg_type < msg_types::USER_MESSAGE_START {
           return Err(NetworkError::InvalidArgument(
               "msg_type must be >= 0x0100".into()
           ));
       }
       if !self.is_leader() {
           return Err(NetworkError::NotLeader);
       }
       let clean_flags = flags & 0xFE;
       let packet = encode_packet(msg_type, payload, clean_flags, self.compression_threshold(), self.max_message_size())?;
       match target {
           ForwardTarget::ToPeer(ref peer_id) => {
               self.transport.send_to_peer(peer_id, packet)?;
           }
           ForwardTarget::Broadcast => {
               self.transport
                   .broadcast(packet, Some(std::slice::from_ref(from_peer_id)))
           }
       }
       Ok(())
   }
   ```

   > 当 Leader 从其他节点接收到 `ForwardedUserData` 时，
   > `SessionRouter::handle_forwarded_user_data` 会被调用。该方法
   > 始终返回一个解包的 `RawPacket` 供本地 `ReceiveCallback` 触发，
   > 并在 `centralized_auto_forward` 为 true 时自动调用
   > `forward_message(..., ForwardTarget::Broadcast, …)` 进行二次
   > 广播。此 helper 简化了接收端逻辑并保证 flags/payload 一致。

6. **运行时配置（注意区分内部方法与 FFI）**：
   - 内部 `SessionRouter` 提供方法：`set_centralized(enable: bool)` / `is_centralized() -> bool` 控制中心化开关。FFI 层同时导出 `SetCentralizedMode` / `IsCentralizedMode` 作为 C 接口。
   - 内部 `SessionRouter` 的 leader 行为可由 `set_auto_forward(enable: bool)` / `is_auto_forward() -> bool` 调整；对应 FFI 为 `SetCentralizedAutoForward` / `IsCentralizedAutoForward`。
   - 内部可通过 `set_compression_threshold(threshold: u32)` 动态调整阈值；对应 FFI 为 `SetCompressionThreshold`，影响后续 `encode_packet` 是否触发 LZ4。

7. **权限检查**：
   - `SendFromLeader`：若本地不是 Leader，返回 `NetworkError::NotLeader`
   - `ForwardMessage`：若本地不是 Leader，返回 `NetworkError::NotLeader`

8. **发送队列溢出处理**：
   - 队列满时返回 `NetworkError::SendQueueFull`
   - FFI 层将错误码返回给 C#，由调用方决定重试或丢弃

### 2. `src/discovery.rs` — 节点发现

**核心结构：**

```rust
pub struct DiscoveryManager {
    daemon: ServiceDaemon,
    local_peer_id: PeerId,
    session_id: String,
    instance_name: String,    // 由 session_id + peer_id 拼接而来
    listen_port: u16,
    membership: Arc<MembershipManager>,
    transport: Arc<TransportManager>,
    shutdown_token: CancellationToken,
}
```

**mDNS 自动发现（优先）：**

1. **服务注册**：

   ```rust
   let service_type = "_meta-mystia._tcp.local.";
   let instance_name = format!("{}_{}", session_id, peer_id);

   let service = ServiceInfo::new(
       service_type,
       &instance_name,
       &format!("{hostname}.local."),
       "",          // 留空，由 mdns-sd 自动探测本机 IP
       listen_port,
       &[
           ("peer_id", peer_id),
           ("session_id", session_id),
           ("protocol_version", protocol_version.as_str()),
       ][..],
   )?;
   daemon.register(service)?
   ```

2. **服务发现**：

   ```rust
   let receiver = daemon.browse(service_type)?;

   tokio::spawn(async move {
       loop {
           tokio::select! {
               _ = shutdown.cancelled() => break,
               // mdns-sd 的 Receiver 不是 async，需用 spawn_blocking 包装
               event = tokio::task::spawn_blocking({
                   let receiver = receiver.clone();
                   move || receiver.recv()
               }) => {
                   let event = match event {
                       Ok(Ok(e)) => e,
                       _ => break,  // JoinError 或 channel 关闭 → 退出
                   };

                   match event {
                       ServiceEvent::ServiceResolved(info) => {
                           let discovered_session = info.get_property_val_str("session_id");
                           let discovered_peer = info.get_property_val_str("peer_id");

                           let (Some(session), Some(peer_id)) = (discovered_session, discovered_peer) else {
                               continue; // TXT 记录不完整，跳过
                           };

                          // 过滤（session 隔离、排除自己、去重、字典序去重、transport 已连接检查）
                          // 委托给 should_connect_to_discovered_peer() 静态方法
                          if !Self::should_connect_to_discovered_peer(
                              &local_peer_id, &session_id, peer_id, session, &membership, &transport,
                           ) {
                               continue;
                           }

                           // 获取地址并异步发起连接
                           if let Some(addr) = info.get_addresses().iter().next() {
                               let target = format!("{}:{}", addr, info.get_port());
                               let transport = Arc::clone(&transport);
                               tokio::spawn(async move {
                                   if let Err(e) = transport.connect_to(&target).await {
                                       tracing::warn!(addr = %target, error = %e, "mDNS auto-connect failed");
                                   }
                               });
                           }
                       }
                       ServiceEvent::ServiceRemoved(_, _) => {
                           // 不主动断开：依赖 Ping/Pong 超时检测
                       }
                       _ => {}
                   }
           }
       }
   }
   ```

3. **服务注销**：`ShutdownNetwork` 时调用 `daemon.unregister(&format!("{}.{}", instance_name, SERVICE_TYPE))` 和 `daemon.shutdown()`

**手动连接：**

1. `ConnectToPeer(addr: &str) -> Result<(), NetworkError>`：
   - 解析 `ip:port` 格式
   - 发起 TCP 连接
   - 执行握手流程（验证 session_id）
   - 若 session_id 不匹配，连接失败

2. `DisconnectPeer(peer_id: &PeerId) -> Result<(), NetworkError>`：
   - 委托给 `TransportManager::disconnect_peer`（见 plan-Transport）

**中心化发现服务器（后续扩展预留）：**

```rust
pub fn connect_to_discovery_server(_server_addr: &str) -> Result<(), NetworkError> {
    Err(NetworkError::NotImplemented)
}
```

---

## Verification

- 非中心化模式广播测试
- 中心化模式：客机消息经 Leader 转发测试
- Leader 转发消息给指定 peer 测试
- 发送队列满时返回正确错误测试
- **msg_type < 0x0100 时发送函数返回 InvalidArgument**
- **用户 flags 的 bit 0 被库自动清除，bit 1-7 透传**
- mDNS 服务注册/发现（同一 session）测试
- 不同 session_id 的节点不互连测试
- `should_connect_to_discovered_peer` 同时检查 `membership.has_peer()` 和 `transport.has_peer()`，任一返回 true 则跳过连接

> **注意**：mDNS 依赖多播（multicast），在 loopback 上可能不工作。mDNS 相关测试需要绕过 mDNS 直接用手动连接，或在有真实网卡的环境中运行，并标记为 `#[ignore]`。

## 实现映射（关键方法与 FFI 对应）

- `SessionRouter` 内部方法（`src/session_router.rs`）：`route_message(target, msg_type, payload, flags)`、`forward_message(from, target, msg_type, flags, payload)`、`send_from_leader(msg_type, payload, flags)`、`set_centralized(bool)`、`set_auto_forward(bool)`、`set_compression_threshold(u32)`、`is_centralized()`。这些内部方法由 `NetworkState` 调用，并由 `src/ffi.rs` 的 FFI 函数间接触发。
- FFI 对应函数（`src/ffi.rs`）：`BroadcastMessage` → 调用 `route_message(MessageTarget::Broadcast, ...)`；`SendToPeer` → `route_message(ToPeer, ...)`；`SendToLeader` → `route_message(ToLeader, ...)`；`SendFromLeader` → `session_router.send_from_leader(...)`；`ForwardMessage` → `session_router.forward_message(...)`；`SetCentralizedMode`/`IsCentralizedMode`、`SetCentralizedAutoForward`/`IsCentralizedAutoForward`、`SetCompressionThreshold` 分别映射到 `SessionRouter` 的 setter/getter。
- mDNS/Discovery 映射：`DiscoveryManager::start()` 在初始化时由 `NetworkState::new` 调用；发现事件会触发 `transport.connect_to(addr)`（异步 spawn）。手动连接 `ConnectToPeer` 在 FFI 中由 `transport.connect_to` 异步执行。

（此段作为路由/发现计划与实际实现的精确映射，便于检核中心化行为与转发语义。）
