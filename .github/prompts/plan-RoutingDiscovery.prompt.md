# Plan: 消息路由与节点发现

> 隶属于 `plan-MetaMystiaNetwork` 总纲 — 阶段 3b：业务层
>
> 依赖：`plan-CoreTypesProtocol`、`plan-Transport`、`plan-MembershipLeader`

本计划覆盖消息路由（含中心化模式）和 mDNS 局域网自动发现。

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
    config: NetworkConfig,
}
```

**功能：**

1. **发送前 msg_type 验证**：
   - 所有用户消息发送函数（`route_message`、`forward_message`）在入口处检查 `msg_type >= USER_MESSAGE_START`（0x0100）
   - 若违反，返回 `NetworkError::InvalidArgument("msg_type must be >= 0x0100")`
   - 防止 C# 误用内部协议消息类型

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
       let packet = encode_packet(msg_type, payload, flags & 0xFE, self.config.compression_threshold)?;
       match target {
           ForwardTarget::ToPeer(peer_id) => {
               self.transport.send_to_peer(&peer_id, packet)?;
           }
           ForwardTarget::Broadcast => {
               self.transport.broadcast(packet, Some(&[from_peer_id.clone()]))?;
           }
       }
       Ok(())
   }
   ```

6. **权限检查**：
   - `SendFromLeader`：若本地不是 Leader，返回 `NetworkError::NotLeader`
   - `ForwardMessage`：若本地不是 Leader，返回 `NetworkError::NotLeader`

7. **发送队列溢出处理**：
   - 队列满时返回 `NetworkError::SendQueueFull`
   - FFI 层将错误码返回给 C#，由调用方决定重试或丢弃

### 2. `src/discovery.rs` — 节点发现

**核心结构：**

```rust
pub struct DiscoveryManager {
    daemon: ServiceDaemon,
    local_peer_id: PeerId,
    session_id: String,
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
       &format!("{}.local.", hostname),
       local_ip,
       listen_port,
       [
           ("peer_id", peer_id),
           ("session_id", session_id),
           ("protocol_version", PROTOCOL_VERSION.to_string()),
       ],
   )?;
   daemon.register(service)?;
   ```

2. **服务发现**：

   ```rust
   let receiver = daemon.browse(service_type)?;

   loop {
       tokio::select! {
           _ = shutdown_token.cancelled() => break,
           event = receiver.recv_async() => {
               match event {
                   Ok(ServiceEvent::ServiceResolved(info)) => {
                       let discovered_session = info.get_property_val_str("session_id");
                       let discovered_peer = info.get_property_val_str("peer_id");

                       let (Some(session), Some(peer_id)) = (discovered_session, discovered_peer) else {
                           continue; // TXT 记录不完整，跳过
                       };

                       // 房间隔离 + 排除自己
                       if session != self.session_id || peer_id == self.local_peer_id.as_str() {
                           continue;
                       }

                       // 去重：已连接则跳过
                       if self.membership.has_peer(&PeerId::from(peer_id)) {
                           continue;
                       }

                       // 防止双向同时连接：peer_id 字典序较小者为主动方
                       if self.local_peer_id.as_str() > peer_id {
                           continue;
                       }

                       // 获取地址并连接
                       if let Some(addr) = info.get_addresses().iter().next() {
                           let target = format!("{}:{}", addr, info.get_port());
                           // spawn 异步连接，不阻塞发现循环
                           let transport = Arc::clone(&self.transport);
                           tokio::spawn(async move {
                               if let Err(e) = transport.connect_to(&target).await {
                                   tracing::warn!(addr = %target, error = %e, "mDNS auto-connect failed");
                               }
                           });
                       }
                   }
                   Ok(ServiceEvent::ServiceRemoved(_, _fullname)) => {
                       // 不主动断开：依赖 Ping/Pong 超时检测
                   }
                   _ => {}
               }
           }
       }
   }
   ```

3. **服务注销**：`ShutdownNetwork` 时调用 `daemon.unregister(instance_name)` 和 `daemon.shutdown()`

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

> **注意**：mDNS 依赖多播（multicast），在 loopback 上可能不工作。mDNS 相关测试需要绕过 mDNS 直接用手动连接，或在有真实网卡的环境中运行，并标记为 `#[ignore]`。
