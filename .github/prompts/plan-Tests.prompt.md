# Plan: 测试

> 隶属于 `plan-MetaMystiaNetwork` 总纲
>
> 覆盖所有模块的单元测试和集成测试。
>
> 注意：测试分两类：一类是针对内部组件（Rust 方法，snake_case）的单元/集成测试；另一类是针对 FFI 导出的生命周期与边界条件测试（CamelCase）。文中分别列出。

---

## 测试清单

| 测试                                       | 验证内容                                                           |
| ------------------------------------------ | ------------------------------------------------------------------ |
| **单元测试**                               |                                                                    |
| `test_packet_encode_decode`                | 消息编码/解码往返 + 各种大小                                       |
| `test_packet_compression`                  | 压缩/解压正确性 + 阈值边界                                         |
| `test_packet_codec_framing`                | PacketCodec 粘包：分包、半包、多包粘连                             |
| `test_internal_message_serde`              | InternalMessage 所有变体序列化往返                                 |
| `test_error_code_mapping`                  | NetworkError → error_code 覆盖所有变体                             |
| `test_config_validation`                   | NetworkConfig::validate 拒绝非法参数                               |
| `test_user_msg_type_validation`            | msg_type < 0x0100 被拒绝                                           |
| `test_flags_compression_bit`               | 用户 flags bit 0 被库清除，bit 1-7 透传                            |
| **连接与握手**                             |                                                                    |
| `test_two_node_connect`                    | 2 节点 loopback TCP 连接 + 握手成功                                |
| `test_handshake_version_mismatch`          | 协议版本不匹配时握手拒绝                                           |
| `test_handshake_session_mismatch`          | session_id 不匹配时握手拒绝                                        |
| `test_handshake_duplicate_peer_id`         | 相同 peer_id 连接时拒绝                                            |
| `test_handshake_timeout`                   | 不发 Handshake，5s 后断开                                          |
| `test_max_connections`                     | 超过 MAX_CONNECTIONS 时拒绝                                        |
| **心跳与成员**                             |                                                                    |
| `test_heartbeat_timeout_detection`         | 心跳超时检测（last_seen 超限后标记 Disconnected）                  |
| `test_rtt_measurement`                     | RTT 计算准确性（MembershipManager.handle_pong 时间差）             |
| `test_reconnect_exponential_backoff`       | 意外断线后自动重连 + 退避                                          |
| `test_disconnect_no_reconnect`             | DisconnectPeer 后不触发自动重连                                    |
| **选举**                                   |                                                                    |
| `test_auto_leader_election_3_nodes`        | 3 节点自动选举                                                     |
| `test_auto_leader_election_2_nodes`        | 2 节点特殊处理（阈值=1）                                           |
| `test_manual_set_leader`                   | SetLeader 覆盖自动选举                                             |
| `test_leader_failover`                     | Leader 掉线后重新选举                                              |
| `test_split_brain_recovery`                | 分区恢复后 Leader 统一                                             |
| **消息路由**                               |                                                                    |
| `test_broadcast_message`                   | 非中心化模式广播                                                   |
| `test_centralized_routing`                 | 中心化模式经 Leader 转发                                           |
| `test_forward_message`                     | Leader 转发给指定 peer                                             |
| `test_send_queue_overflow`                 | 队列满时返回 SendQueueFull                                         |
| `test_large_message_near_limit`            | 接近 MAX_MESSAGE_SIZE 的消息                                       |
| `test_message_too_large`                   | 超 MAX_MESSAGE_SIZE 时拒绝                                         |
| **发现**                                   |                                                                    |
| `test_discovery_same_session_auto_connect` | mDNS 注册/发现（同 session）                                       |
| `test_discovery_session_isolation`         | 不同 session 不互连                                                |
| **FFI 与生命周期**                         |                                                                    |
| `test_ffi_callbacks`                       | 回调注册与触发                                                     |
| `test_ffi_error_codes_not_initialized`     | 未初始化时调用 FFI 返回正确错误码                                  |
| `test_ffi_full_lifecycle`                  | FFI 完整生命周期：initialize→connect→send→shutdown                 |
| `test_concurrent_ffi_calls`                | 多线程 FFI 调用安全                                                |
| `test_graceful_shutdown`                   | PeerLeave + 资源清理                                               |
| `test_shutdown_reinitialize`               | Shutdown 后可再次 Initialize                                       |
| `test_rapid_connect_disconnect`            | 快速连接/断开压力测试                                              |
| `test_broadcast_lagged_recovery`           | broadcast channel Lagged 后仍能通过 get_peer_list 获取完整成员列表 |

> mDNS 测试依赖多播，loopback 上可能不工作。标记 `#[ignore]`，在有真实网卡的环境运行。

---

## 测试辅助

所有集成测试均显定义在 `src/lib.rs` 的 `#[cfg(test)]` 模块中，使用以下三个现成辅助函数：

```rust
// 创建一个节点
// NetworkConfig::default()
async fn create_node(peer_id: &str, session_id: &str) -> NetworkState {
    NetworkState::new(
        peer_id.to_string(),
        session_id.to_string(),
        NetworkConfig::default(),
    )
    .await
    .unwrap()
}

// 连接两个节点：a 主动连接 b
async fn connect_nodes(a: &NetworkState, b: &NetworkState) {
    let listen = b.transport.listener_addr();
    let addr = format!("127.0.0.1:{}", listen.port());
    a.transport.connect_to(&addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
}

// 持续轮询直到条件成立或超时
async fn wait_until(f: impl Fn() -> bool, timeout_ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    while tokio::time::Instant::now() < deadline {
        if f() { return true; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}
```

测试使用 `#[tokio::test]`，`loopback 127.0.0.1` 模拟多节点。

---

## Verification

- 所有测试在 Windows 和 macOS 上通过
- `cargo test` + `cargo test --release` 均无失败
- mDNS 测试在真实网卡环境通过（`cargo test -- --ignored`）

## 测试级别映射（内部 vs FFI）

- **内部测试（Rust API / 单元测试）**：如 `test_packet_encode_decode`、`test_packet_compression`、`test_packet_codec_framing`、`test_internal_message_serde`、`test_config_validation` 等，直接调用 crate 内部函数（snake_case）并使用 `#[tokio::test]` / `#[test]`。
- **FFI 测试（生命周期/边界）**：如 `test_ffi_callbacks`、`test_ffi_error_codes_not_initialized`、`test_ffi_full_lifecycle`、`test_concurrent_ffi_calls`、`test_shutdown_reinitialize` 等，通过 `InitializeNetwork` / `ShutdownNetwork` / `BroadcastMessage` 等导出函数进行集成级别验证，确保 `catch_unwind`、字符串返回语义、回调线程行为与错误码契约（其中 panic 安全性与握手超时在 `test_ffi_full_lifecycle` 中一并覆盖）。
- **混合集成测试**：如 `test_two_node_connect`、`test_auto_leader_election_3_nodes` 等使用 `NetworkState::new` 或在钩子里直接 spawn 多个 runtime 节点以模拟网络拓扑；这些测试更接近实现细节并用于回归验证。

（此节帮助区分应该新增到哪一类测试以覆盖特定实现边界。）
