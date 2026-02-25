# Plan: 测试

> 隶属于 `plan-MetaMystiaNetwork` 总纲
>
> 覆盖所有模块的单元测试和集成测试。

---

## 测试清单

| 测试                                 | 验证内容                                |
| ------------------------------------ | --------------------------------------- |
| **单元测试**                         |                                         |
| `test_packet_encode_decode`          | 消息编码/解码往返 + 各种大小            |
| `test_packet_compression`            | 压缩/解压正确性 + 阈值边界              |
| `test_packet_codec_framing`          | PacketCodec 粘包：分包、半包、多包粘连  |
| `test_internal_message_serde`        | InternalMessage 所有变体序列化往返      |
| `test_error_code_mapping`            | NetworkError → error_code 覆盖所有变体  |
| `test_config_validation`             | NetworkConfig::validate 拒绝非法参数    |
| `test_user_msg_type_validation`      | msg_type < 0x0100 被拒绝                |
| `test_flags_compression_bit`         | 用户 flags bit 0 被库清除，bit 1-7 透传 |
| **连接与握手**                       |                                         |
| `test_two_node_connect`              | 2 节点 loopback TCP 连接 + 握手成功     |
| `test_handshake_version_mismatch`    | 协议版本不匹配时握手拒绝                |
| `test_handshake_session_mismatch`    | session_id 不匹配时握手拒绝             |
| `test_handshake_duplicate_peer_id`   | 相同 peer_id 连接时拒绝                 |
| `test_handshake_timeout`             | 不发 Handshake，5s 后断开               |
| `test_max_connections`               | 超过 MAX_CONNECTIONS 时拒绝             |
| **心跳与成员**                       |                                         |
| `test_heartbeat_detection`           | 心跳发送/接收/超时检测                  |
| `test_rtt_measurement`               | RTT 计算准确性                          |
| `test_reconnect_exponential_backoff` | 意外断线后自动重连 + 退避               |
| `test_disconnect_no_reconnect`       | DisconnectPeer 后不触发自动重连         |
| **选举**                             |                                         |
| `test_auto_leader_election_3_nodes`  | 3 节点自动选举                          |
| `test_auto_leader_election_2_nodes`  | 2 节点特殊处理（阈值=1）                |
| `test_manual_leader_set`             | SetLeader 覆盖自动选举                  |
| `test_leader_failover`               | Leader 掉线后重新选举                   |
| `test_split_brain_recovery`          | 分区恢复后 Leader 统一                  |
| **消息路由**                         |                                         |
| `test_broadcast_message`             | 非中心化模式广播                        |
| `test_centralized_routing`           | 中心化模式经 Leader 转发                |
| `test_forward_message`               | Leader 转发给指定 peer                  |
| `test_send_queue_overflow`           | 队列满时返回 SendQueueFull              |
| `test_large_message`                 | 接近 MAX_MESSAGE_SIZE 的消息            |
| `test_message_too_large`             | 超 MAX_MESSAGE_SIZE 时拒绝              |
| **发现**                             |                                         |
| `test_mdns_discovery`                | mDNS 注册/发现（同 session）            |
| `test_mdns_session_isolation`        | 不同 session 不互连                     |
| **FFI 与生命周期**                   |                                         |
| `test_ffi_callbacks`                 | 回调注册与触发                          |
| `test_ffi_error_codes`               | FFI 返回正确错误码                      |
| `test_ffi_panic_safety`              | panic 不崩溃，返回 INTERNAL_ERROR       |
| `test_concurrent_ffi_calls`          | 多线程 FFI 调用安全                     |
| `test_graceful_shutdown`             | PeerLeave + 资源清理                    |
| `test_shutdown_reinitialize`         | Shutdown 后可再次 Initialize            |
| `test_rapid_connect_disconnect`      | 快速连接/断开压力测试                   |
| `test_broadcast_lagged_recovery`     | broadcast Lagged 后状态同步             |

> mDNS 测试依赖多播，loopback 上可能不工作。标记 `#[ignore]`，在有真实网卡的环境运行。

---

## 测试辅助

```rust
async fn create_test_nodes(count: usize, session_id: &str) -> Vec<NetworkState> {
    let mut nodes = Vec::with_capacity(count);
    for i in 0..count {
        let node = NetworkState::new(
            format!("test_peer_{}", i),
            session_id.to_string(),
            NetworkConfig::default(),
        ).await.unwrap();
        nodes.push(node);
    }
    nodes
}

async fn wait_for_full_mesh(nodes: &[NetworkState], timeout: Duration) { /* 轮询直到所有节点互相连接 */ }
async fn wait_for_leader(nodes: &[NetworkState], timeout: Duration) -> PeerId { /* 轮询直到所有节点同意同一 Leader */ }
```

测试使用 `#[tokio::test]`，loopback `127.0.0.1` 模拟多节点。

---

## Verification

- 所有测试在 Windows 和 macOS 上通过
- `cargo test` + `cargo test --release` 均无失败
- mDNS 测试在真实网卡环境通过（`cargo test -- --ignored`）
