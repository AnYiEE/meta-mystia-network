# Plan: 测试

> 隶属于 `plan-MetaMystiaNetwork` 总纲
>
> 覆盖所有模块的单元测试和集成测试。
>
> 注意：测试分两类：一类是针对内部组件（Rust 方法，snake_case）的单元/集成测试；另一类是针对 FFI 导出的生命周期与边界条件测试（CamelCase）。文中分别列出。

---

## 测试清单

| 测试                                                          | 验证内容                                                               |
| ------------------------------------------------------------- | ---------------------------------------------------------------------- |
| **单元测试**                                                  |                                                                        |
| `test_packet_encode_decode`                                   | 消息编码/解码往返 + 各种大小                                           |
| `test_packet_compression`                                     | 压缩/解压正确性 + 阈值边界                                             |
| `test_packet_codec_framing`                                   | PacketCodec 粘包：分包、半包、多包粘连                                 |
| `test_internal_message_serde`                                 | InternalMessage 所有变体序列化往返                                     |
| `test_error_code_mapping`                                     | NetworkError → error_code 覆盖所有变体                                 |
| `test_already_connected_error_code_is_negative_14`            | `AlreadyConnected` 错误码映射为 -14                                    |
| `test_validation`                                             | NetworkConfig::validate 拒绝非法参数                                   |
| `test_user_msg_type_validation`                               | msg_type < 0x0100 被拒绝                                               |
| `test_flags_compression_bit`                                  | 用户 flags bit 0 被库清除，bit 1-7 透传                                |
| `test_manual_override_recovery_from_u8`                       | `ManualOverrideRecovery::from_u8` 正确映射 0/1/其他值                  |
| `test_manual_override_recovery_default_in_config`             | `NetworkConfig::default()` 的 `manual_override_recovery` 为 `Hold`     |
| `test_tcp_nodelay_default_in_config`                          | `NetworkConfig::default()` 的 `tcp_nodelay` 为 `true`                  |
| `test_peer_id_new_and_as_str`                                 | `PeerId::new` 与 `as_str` 往返一致                                     |
| `test_peer_id_display`                                        | `PeerId` 的 `Display` 输出与内容一致                                   |
| `test_peer_id_from_str`                                       | `PeerId::from(&str)` 正确构造                                          |
| `test_peer_id_from_string`                                    | `PeerId::from(String)` 正确构造                                        |
| `test_peer_id_as_ref`                                         | `PeerId` 的 `AsRef<str>` 正确返回                                      |
| `test_peer_id_eq_hash`                                        | `PeerId` 的 `Eq` / `Hash` 一致性                                       |
| `test_peer_status_as_i32`                                     | `PeerStatus` 所有变体的 `as_i32` 映射正确                              |
| `test_peer_info_new_defaults`                                 | `PeerInfo::new` 默认 status、rtt、last_seen 正确                       |
| `test_error_display`                                          | `NetworkError` 所有变体的 `Display` 输出非空                           |
| `test_error_from_io`                                          | `io::Error` → `NetworkError::Io` 转换                                  |
| `test_error_from_postcard`                                    | `postcard::Error` → `NetworkError::Serialization` 转换                 |
| `test_error_source`                                           | `NetworkError::source()` 返回正确内部错误                              |
| `test_c_str_to_string_null`                                   | FFI `c_str_to_string` 传入 null 返回空字符串                           |
| `test_return_string_and_last_error`                           | `return_string` 设置 `LAST_ERROR` 并可通过 `GetLastError` 读取         |
| `test_registration_and_delivery`                              | 回调注册后 `invoke_*` 正确投递                                         |
| `test_unregister_and_shutdown`                                | 回调注销/shutdown 后不再投递                                           |
| **连接与握手**                                                |                                                                        |
| `test_two_node_connect`                                       | 2 节点 loopback TCP 连接 + 握手成功                                    |
| `test_tcp_nodelay_connect_and_exchange`                       | `tcp_nodelay: true` 时两节点可正常连接并收发消息                       |
| `test_handshake_version_mismatch`                             | 协议版本不匹配时握手拒绝                                               |
| `test_handshake_session_mismatch`                             | session_id 不匹配时握手拒绝                                            |
| `test_handshake_duplicate_peer_id`                            | 相同 peer_id 连接时拒绝                                                |
| `test_handshake_timeout`                                      | 不发 Handshake，5s 后断开                                              |
| `test_max_connections`                                        | 超过 `config.max_connections`（`NetworkConfig` 字段）时拒绝            |
| **心跳与成员**                                                |                                                                        |
| `test_add_remove_peer`                                        | MembershipManager 添加/移除 peer 正确更新集合                          |
| `test_status_update`                                          | MembershipManager 状态更新生效                                         |
| `test_heartbeat_timeout_detection`                            | 心跳超时检测（last_seen 超限后标记 Disconnected）                      |
| `test_rtt_measurement`                                        | RTT 计算准确性（MembershipManager.handle_pong 时间差）                 |
| `test_update_last_seen_prevents_timeout`                      | 更新 last_seen 后超时检测不触发                                        |
| `test_membership_events`                                      | MembershipManager 事件通知（Joined/Left）正确发出                      |
| `test_get_connected_peers`                                    | `get_connected_peers` 只返回 Connected 状态的 peer                     |
| `test_reconnect_exponential_backoff`                          | 意外断线后自动重连 + 退避                                              |
| `test_disconnect_no_reconnect`                                | DisconnectPeer 后不触发自动重连                                        |
| **选举**                                                      |                                                                        |
| `test_initial_state`                                          | LeaderElection 初始状态（Follower, term=0, 无 leader）                 |
| `test_start_election_single_node`                             | 单节点自动选举立即成为 Leader                                          |
| `test_start_election_two_nodes`                               | 2 节点选举阈值 = 1                                                     |
| `test_start_election_three_nodes`                             | 3 节点选举阈值 = 2                                                     |
| `test_vote_response_majority`                                 | 收到多数投票后晋升 Leader                                              |
| `test_handle_heartbeat`                                       | 收到合法心跳后更新 leader_id 和 term                                   |
| `test_higher_term_steps_down`                                 | 收到更高 term 时降为 Follower                                          |
| `test_auto_leader_election_3_nodes`                           | 3 节点自动选举                                                         |
| `test_auto_leader_election_2_nodes`                           | 2 节点特殊处理（阈值=1）                                               |
| `test_manual_set_leader`                                      | SetLeader 覆盖自动选举                                                 |
| `test_manual_set_leader_self`                                 | SetLeader 设置自身为 Leader                                            |
| `test_majority_threshold`                                     | `majority_threshold` 计算正确（5 节点 → 3）                            |
| `test_heartbeat_rejects_foreign_leader_under_manual_override` | `manual_override=true` 时拒绝非当前 leader 的心跳                      |
| `test_heartbeat_accepts_same_leader_under_manual_override`    | `manual_override=true` 时接受当前 leader 的心跳                        |
| `test_peer_left_hold_recovery_keeps_manual_override`          | `ManualOverrideRecovery::Hold` 下 leader 掉线保持 manual_override      |
| `test_peer_left_auto_elect_recovery_clears_manual_override`   | `ManualOverrideRecovery::AutoElect` 下 leader 掉线清除 manual_override |
| `test_leader_failover`                                        | Leader 掉线后重新选举                                                  |
| `test_split_brain_recovery`                                   | 分区恢复后 Leader 统一                                                 |
| `test_vote_rejected_lower_term`                               | 低 term 投票请求被拒绝                                                 |
| `test_vote_rejected_already_voted`                            | 同 term 重复投票被拒绝                                                 |
| `test_manual_override_blocks_election`                        | manual_override 阻止自动选举                                           |
| `test_enable_auto_election_preserves_override_with_leader`    | 有 leader 时 enable_auto_election 保留 override                        |
| `test_enable_auto_election_after_leader_gone`                 | leader 消失后 enable_auto_election 清除 override                       |
| `test_leader_assign`                                          | `assign_leader` 正确设置 leader_id 和 role                             |
| **消息路由**                                                  |                                                                        |
| `test_broadcast_message`                                      | 非中心化模式广播                                                       |
| `test_centralized_routing`                                    | 中心化模式经 Leader 转发                                               |
| `test_forward_message`                                        | Leader 转发给指定 peer                                                 |
| `test_send_queue_overflow`                                    | 队列满时返回 SendQueueFull                                             |
| `test_large_message_near_limit`                               | 接近 `max_message_size`（`NetworkConfig` 字段）的消息                  |
| `test_message_too_large`                                      | 超 `max_message_size` 时拒绝                                           |
| `test_compression_not_beneficial`                             | 不可压缩数据保留原始格式                                               |
| `test_compression_threshold_boundary`                         | 恰好等于/超过压缩阈值时的行为                                          |
| `test_default_config_valid`                                   | `NetworkConfig::default()` 通过 `validate()` 检查                      |
| `test_internal_message_partial_eq`                            | `InternalMessage` 的 `PartialEq` 正确区分变体和字段值                  |
| `test_msg_type_returns_internal_range`                        | 所有 `InternalMessage` 变体的 `msg_type()` < `USER_MESSAGE_START`      |
| `test_msg_type_constants_unique`                              | `msg_types::all()` 中无重复常量                                        |
| `test_msg_type_all_covers_every_constant`                     | `msg_types::all()` 数量与枚举变体数一致                                |
| `test_msg_type_expected_values`                               | 特定变体返回预期的 `msg_type` 值                                       |
| `test_packet_codec_partial`                                   | PacketCodec 处理不完整的 header 和 payload                             |
| `test_packet_codec_multiple`                                  | PacketCodec 处理多个粘连数据包                                         |
| `test_packet_codec_too_large`                                 | PacketCodec 拒绝超过 max_message_size 的包                             |
| **发现**                                                      |                                                                        |
| `test_discovery_same_session_auto_connect`                    | mDNS 注册/发现（同 session）                                           |
| `test_discovery_session_isolation`                            | 不同 session 不互连                                                    |
| `test_same_session_connects`                                  | 同 session 的 discovered peer 应连接（单元级）                         |
| `test_different_session_rejects`                              | 不同 session 的 discovered peer 应拒绝（单元级）                       |
| `test_self_excluded`                                          | 自身 peer_id 不触发自动连接                                            |
| `test_already_connected_skipped`                              | 已连接的 peer 不重复发起连接                                           |
| `test_lexicographic_dedup`                                    | 字典序去重避免双向连接                                                 |
| `test_transport_connected_skipped`                            | `should_connect_to_discovered_peer` 在 transport 已连接时跳过          |
| `test_connect_to_discovery_server_not_implemented`            | `connect_to_discovery_server` 返回 NotImplemented                      |
| **AlreadyConnected 与竞态保护**                               |                                                                        |
| `test_already_connected_inbound`                              | 入站重复连接返回 AlreadyConnected（transport 层）                      |
| `test_already_connected_outbound`                             | 出站重复连接返回 AlreadyConnected（transport 层）                      |
| **双通道与连接健壮性**                                        |                                                                        |
| `test_data_channel_established_after_connect`                 | 控制通道连接后数据通道自动建立                                         |
| `test_dual_channel_message_routing`                           | 用户消息走数据通道，内部消息走控制通道                                 |
| `test_socket_linger_configured`                               | `configure_socket` 设置 SO_LINGER = 2s                                 |
| **FFI 与生命周期**                                            |                                                                        |
| `test_ffi_callbacks`                                          | 回调注册与触发                                                         |
| `test_ffi_error_codes_not_initialized`                        | 未初始化时调用 FFI 返回正确错误码                                      |
| `test_ffi_full_lifecycle`                                     | FFI 完整生命周期：initialize→connect→send→shutdown                     |
| `test_ffi_tcp_nodelay_roundtrip`                              | FFI `tcp_nodelay` 字段 0/1 → `NetworkConfig::tcp_nodelay` bool 转换    |
| `test_ffi_config_size_unchanged`                              | `NetworkConfigFFI` 结构体大小保持 40 字节                              |
| `test_keepalive_defaults`                                     | 默认 keepalive 配置值为 60/10/3                                        |
| `test_keepalive_validation`                                   | keepalive 字段为 0 时校验失败                                          |
| `test_ffi_keepalive_roundtrip`                                | FFI keepalive 字段往返映射正确                                         |
| `test_custom_keepalive_connect_and_exchange`                  | 自定义 keepalive 参数下节点连接与消息收发                              |
| `test_concurrent_ffi_calls`                                   | 多线程 FFI 调用安全                                                    |
| `test_graceful_shutdown`                                      | PeerLeave + 资源清理                                                   |
| `test_shutdown_reinitialize`                                  | Shutdown 后可再次 Initialize                                           |
| `test_rapid_connect_disconnect`                               | 快速连接/断开压力测试                                                  |
| `test_broadcast_lagged_recovery`                              | broadcast channel Lagged 后仍能通过 get_peer_list 获取完整成员列表     |

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

- **内部测试（Rust API / 单元测试）**：如 `test_packet_encode_decode`、`test_packet_compression`、`test_packet_codec_framing`、`test_internal_message_serde`、`test_validation` 等，直接调用 crate 内部函数（snake_case）并使用 `#[tokio::test]` / `#[test]`。
- **FFI 测试（生命周期/边界）**：如 `test_ffi_callbacks`、`test_ffi_error_codes_not_initialized`、`test_ffi_full_lifecycle`、`test_concurrent_ffi_calls`、`test_shutdown_reinitialize` 等，通过 `InitializeNetwork` / `ShutdownNetwork` / `BroadcastMessage` 等导出函数进行集成级别验证，确保 `catch_unwind`、字符串返回语义、回调线程行为与错误码契约（其中 panic 安全性与握手超时在 `test_ffi_full_lifecycle` 中一并覆盖）。
- **混合集成测试**：如 `test_two_node_connect`、`test_auto_leader_election_3_nodes` 等使用 `NetworkState::new` 或在钩子里直接 spawn 多个 runtime 节点以模拟网络拓扑；这些测试更接近实现细节并用于回归验证。

## E2E 集成测试

| 测试名                                        | 模块                                   | 验证内容                                                      |
| --------------------------------------------- | -------------------------------------- | ------------------------------------------------------------- |
| `test_unicast_delivery_verified`              | `e2e_unicast_delivery`                 | 单播消息端到端可达                                            |
| `test_broadcast_delivery_verified`            | `e2e_broadcast_delivery`               | 广播消息所有节点均收到                                        |
| `test_bidirectional_messaging`                | `e2e_bidirectional`                    | 双向消息收发正确                                              |
| `test_sequential_delivery`                    | `e2e_sequential_messages`              | 顺序消息保序到达                                              |
| `test_large_payload_delivery`                 | `e2e_large_payload`                    | 大载荷消息端到端传输                                          |
| `test_type_and_flags_preserved`               | `e2e_msg_type_and_flags`               | msg_type 和 flags 端到端透传保留                              |
| `test_full_mesh_broadcast`                    | `e2e_full_mesh`                        | 全网状拓扑广播覆盖所有节点                                    |
| `test_status_callbacks_on_connect_disconnect` | `e2e_peer_status_callbacks`            | 连接/断开触发状态回调                                         |
| `test_reconnect_then_message`                 | `e2e_reconnect_messaging`              | 断线重连后消息仍可达                                          |
| `test_centralized_broadcast_delivery`         | `e2e_centralized_routing`              | 中心化模式经 Leader 广播到达所有 follower                     |
| `test_leader_broadcast_delivery`              | `e2e_leader_broadcast`                 | Leader 广播消息到达所有 follower                              |
| `test_late_joiner_receives_messages`          | `e2e_dynamic_join`                     | 后加入节点能收到后续消息                                      |
| `test_mdns_discovers_and_connects`            | `e2e_mdns_auto_discovery`              | mDNS 发现后自动建立连接                                       |
| `test_mdns_different_session_no_connect`      | `e2e_mdns_cross_session_no_connect`    | 不同 session 的 mDNS 节点不互连                               |
| `test_mdns_three_node_auto_mesh`              | `e2e_mdns_three_node_mesh`             | 3 节点 mDNS 自动全网状连接                                    |
| `test_duplicate_peer_id_connection_rejected`  | `e2e_duplicate_peer_id_rejected`       | 重复 peer_id 连接被拒绝                                       |
| `test_peer_list_sync_auto_mesh`               | `e2e_peer_list_sync_auto_mesh`         | peer list 同步驱动自动组网                                    |
| `test_concurrent_senders_all_delivered`       | `e2e_concurrent_senders`               | 多个并发发送方消息全部到达                                    |
| `test_empty_payload_delivery`                 | `e2e_empty_payload`                    | 空载荷消息正常传输                                            |
| `test_all_nodes_agree_on_leader`              | `e2e_leader_election_consensus`        | 所有节点 leader 一致                                          |
| `test_graceful_leave_propagates`              | `e2e_graceful_leave_detection`         | 优雅离开消息传播到所有节点                                    |
| `test_connection_refused_graceful`            | `e2e_connect_refused`                  | 连接被拒绝时优雅处理                                          |
| `test_leader_failover_new_election`           | `e2e_leader_failover`                  | Leader 掉线后触发新选举                                       |
| `test_send_to_leader_and_from_leader`         | `e2e_send_to_from_leader`              | SendToLeader / SendFromLeader 端到端验证                      |
| `test_forward_message_delivery_to_follower`   | `e2e_forward_message_delivery`         | Leader 转发消息到指定 follower                                |
| `test_compression_roundtrip`                  | `e2e_lz4_compression_verified`         | LZ4 压缩数据端到端往返正确                                    |
| `test_keepalive_timeout_detection`            | `e2e_keepalive_timeout`                | Keepalive 超时后断开并通知                                    |
| `test_max_connections_rejection`              | `e2e_max_connections_enforced`         | 超过 max_connections 时拒绝新连接                             |
| `test_version_mismatch_then_recovery`         | `e2e_version_mismatch`                 | 协议版本不匹配后可恢复连接                                    |
| `test_ffi_operations_before_init`             | `e2e_ffi_wrong_order`                  | 未初始化时 FFI 操作全部返回错误码不崩溃                       |
| `test_rapid_reconnect_then_message`           | `e2e_rapid_reconnect_stability`        | 快速重连后消息仍可达                                          |
| `test_mdns_race_connect_then_manual`          | `e2e_mdns_race_connect_then_manual`    | mDNS 自动连接后手动 ConnectTo 返回 AlreadyConnected（非致命） |
| `test_election_skips_known_leader`            | `e2e_election_skips_known_leader`      | 已知 leader 时选举 term 稳定不变，无多余选举                  |
| `test_manual_leader_survives_heartbeat`       | `e2e_manual_leader_survives_heartbeat` | 手动 SetLeader 后节点拒绝外部高 term 心跳，不被覆盖           |
