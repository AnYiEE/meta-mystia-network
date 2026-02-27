# Plan: 成员管理与 Leader 选举

> 隶属于 `plan-MetaMystiaNetwork` 总纲 — 阶段 3a：业务层
>
> 依赖：`plan-CoreTypesProtocol`、`plan-Transport`

本计划覆盖 Peer 列表管理、存活探测、RTT 测量和简化 Raft Leader Election。

注意：本文件同时描述内部 Rust 实现（snake_case）与 FFI 导出（CamelCase），并在上下文中说明对应关系。

---

## Steps

### 1. `src/membership.rs` — 成员管理

**核心结构：**

```rust
pub struct MembershipManager {
    local_peer_id: PeerId,
    peers: Arc<RwLock<HashMap<PeerId, PeerInfo>>>,
    config: NetworkConfig,
    /// 成员变更事件发送端（broadcast channel，容量 256，支持多个订阅者）
    /// LeaderElection、CallbackManager 各调用 event_tx.subscribe() 获取独立 receiver
    pub event_tx: broadcast::Sender<MembershipEvent>,
}

#[derive(Debug, Clone)]
pub enum MembershipEvent {
    Joined(PeerId),
    Left(PeerId),
    StatusChanged { peer_id: PeerId, old: PeerStatus, new: PeerStatus },
}
```

**功能：**

1. **Peer 列表维护**：
   - `add_peer(peer_id, addr) -> Result<(), NetworkError>`：添加前检查 peer_id 是否已存在，发送 `Joined` 事件
   - `remove_peer(peer_id)`：移除 peer 并发送 `Left` 事件
   - `update_status(peer_id, status)`：更新状态并发送事件
   - `has_peer(peer_id) -> bool`：检查 peer 是否存在（用于去重检查）
   - `get_peer_list() -> Vec<PeerInfo>`：返回所有 peer 的克隆列表
   - `get_connected_peers() -> Vec<PeerId>`：仅返回 Connected 状态的 peer
   - `get_peer_rtt(peer_id) -> Option<u32>`：获取指定 peer 的 RTT
   - `get_peer_status(peer_id) -> Option<PeerStatus>`：获取指定 peer 的状态
   - `get_connected_peer_count() -> usize`：返回 Connected 状态的 peer 数量（不含自己）
   - `update_last_seen(peer_id)`：仅刷新指定 peer 的存活时间戳，Transport 在收到任何用户或内部消息时调用；与 `handle_pong` 配合使用。

2. **Peer ID 冲突检测**：
   - 握手时检查 `peer_id` 是否已存在于连接列表
   - 若冲突，返回 `HandshakeAck { success: false, error_reason: Some("duplicate_peer_id") }`

3. **存活探测（Ping/Pong）**：
   - **主动发送**：每 `heartbeat_interval_ms`（默认 500ms）向所有 Connected 状态的 peer 发送 `Ping { timestamp_ms }`（由消息循环发起，非 MembershipManager 直接发送）
   - **被动检测**：收到 `Ping` 时回复 `Pong`；收到 `Pong` 时更新 `last_seen` 和 `rtt_ms`
   - **超时判断**：后台 task 每 `heartbeat_interval_ms` 检查一次，若 `last_seen` 距今超过 `heartbeat_interval * timeout_multiplier`，标记为 Disconnected
   - Raft `Heartbeat` 仅由 Leader 发送（见 leader.rs），仅用于维持领导权，不影响存活判定

4. **RTT 测量**：

   ```rust
   fn handle_pong(&self, peer_id: &PeerId, sent_timestamp_ms: u64) {
       let now = current_timestamp_ms();
       let rtt = now.saturating_sub(sent_timestamp_ms) as u32;
       if let Some(peer) = self.peers.write().get_mut(peer_id) {
           peer.rtt_ms = Some(rtt);
           peer.last_seen = Instant::now();
       }
   }
   ```

5. **Peer 上/下线事件**：
   - 通过 `broadcast::Receiver<MembershipEvent>` 订阅
   - `LeaderElection`、`CallbackManager` 各调用 `event_tx.subscribe()` 获取独立 receiver
   - FFI 层通过 `RegisterPeerStatusCallback` 注册 C# 回调

6. **broadcast channel Lagged 容错**：
   - `tokio::sync::broadcast` 容量 256，若 receiver 消费速度跟不上 sender，会触发 `RecvError::Lagged(n)` 丢弃最旧的 n 条消息
   - **处理策略**：receiver 收到 `Lagged` 时，记录 `warn!` 日志并退出当前 drain 循环（不做全量同步，成员状态可通过 `get_peer_list()` 单独获取）
   - 这确保 LeaderElection 不会因错过 `PeerLeft` 事件而保留过期的 peer 计数

### 2. `src/leader.rs` — Leader 选举

**简化 Raft 实现**（仅 Leader Election 部分，~400 行）

**核心状态：**

```rust
struct ElectionState {
    role: Role,
    current_term: u64,
    voted_for: Option<PeerId>,
    leader_id: Option<PeerId>,
    votes_received: HashSet<PeerId>,
    auto_election_enabled: bool,
    manual_override: bool,
}

pub struct LeaderElection {
    local_peer_id: PeerId,
    state: RwLock<ElectionState>,
    membership_rx: Mutex<broadcast::Receiver<MembershipEvent>>,
    leader_change_tx: mpsc::Sender<Option<PeerId>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}
```

**时间配置：**

| 参数         | 值          | 说明                    |
| ------------ | ----------- | ----------------------- |
| 心跳间隔     | 500ms       | Leader 发送心跳的频率   |
| 选举超时     | 1500-3000ms | 随机化，为心跳的 3-6 倍 |
| 心跳超时判定 | 1500ms      | 3 个心跳周期未收到      |

**选举流程（当 auto_election 启用时）：**

1. **Follower** 状态：
   - 等待 Leader 的 Raft `Heartbeat`
   - 若选举超时（随机 1500-3000ms）到期无心跳，转为 Candidate
   - 收到更高 term 的消息时，更新 term，**重置 `voted_for = None`**，保持 Follower

2. **Candidate** 状态：
   - 进入 Candidate 时的初始化步骤：
     1. `term += 1`
     2. 重置 `voted_for = None`，然后立即设为 `voted_for = self`（投票给自己）
     3. 清空 `votes_received`，加入自己
     4. 重置选举超时（随机 1500-3000ms）
   - 向所有 Connected peers 发送 `RequestVote { term, candidate_id }`
   - 若获得多数票（`votes_received.len() >= majority_threshold`）→ 成为 Leader
   - 若收到更高 term 消息 → 更新 term，重置 `voted_for = None`，退回 Follower
   - 若选举超时 → 重新执行上述初始化步骤（term 再 +1）

3. **Leader** 状态：
   - 立即发送 Raft `Heartbeat { term, leader_id }` 宣告领导权
   - 每 500ms 发送 Raft `Heartbeat` 维持领导权
   - 若收到更高 term 消息则 **更新 term，重置 `voted_for = None`**，退回 Follower

4. **投票规则**（收到 `RequestVote` 时）：
   - 若请求的 term < 当前 term，拒绝（`granted = false`）
   - 若请求的 term > 当前 term，更新 term，**重置 `voted_for = None`**，退回 Follower
   - 若 `voted_for` 为 None 或等于 candidate_id，投赞成票（`granted = true`），设置 `voted_for = candidate_id`
   - 否则拒绝（已投给其他候选人）

**多数派阈值计算：**

```rust
/// total_nodes = 网络中的总节点数（含自己）
///             = 1 (self) + connected_peer_count
fn majority_threshold(total_nodes: usize) -> usize {
    if total_nodes <= 2 {
        // 1 节点：自己即为 Leader（阈值 1，自投即达成）
        // 2 节点：任一节点存活即可为 Leader（阈值 1），
        //         避免另一方掉线后无法选举
        1
    } else {
        // 3+ 节点：标准多数派
        total_nodes / 2 + 1
    }
}
```

> **语义说明**：`total_nodes` 包含自己。例如 3 人房间中 `total_nodes = 3`，阈值 = 2，Candidate 自投 + 1 票即可当选。

**网络分区恢复（Split-brain）处理：**

- 分区恢复后可能出现多个 Leader
- **高 term 优先**：收到更高 term 心跳的旧 Leader 立即退回 Follower
- **同 term 平局**（仅 ≤2 节点时可能出现，因为 ≥3 节点需多数票不可能同 term 双 Leader）：peer_id 字典序较大者为 Leader，较小者退回 Follower。这提供了确定性的收敛，无需等待超时重选。

**手动覆盖（`SetLeader`）：**

- 设置 `manual_override = true`
- 本地立即更新 `leader_id`
- 向所有 peer 广播 `LeaderAssign { term: current_term + 1, leader_id, assigner_id }`
- 所有节点收到后：
  - 更新 `term` 和 `leader_id`
  - 设置 `manual_override = true`
  - 不再发起自动选举
- 当手动指定的 Leader 掉线且 `EnableAutoLeaderElection(true)` 被调用时，清除 `manual_override` 并恢复自动选举

**Leader 变更回调**：

- 当 `leader_id` 变化时（包括从 `Some` 到 `None`），发送事件到 `leader_change_tx`
- FFI 层订阅此 channel 并调用 C# 回调

**broadcast Lagged 处理**：

- `membership_rx` 收到 `Lagged` 时，记录 `warn!` 日志并退出当前 drain 循环（不做实际全量同步）
- 实际节点计数由 `drain_membership_events` 的调用方在外层维护，Lagged 时 `connected_peer_count` 可能偏小，选举误匹配 不影响正确性只影响决策时机

---

## Verification

- 选举状态机单元测试：Follower → Candidate → Leader 状态转换
- 3 节点自动选举集成测试
- 2 节点特殊处理测试（阈值 = 1，自投即当选）
- 手动 `SetLeader` 覆盖自动选举测试
- Leader 掉线后自动重新选举测试
- 网络分区恢复后 Leader 统一测试（高 term 优先 + 同 term 字典序平局）
- 心跳发送/接收/超时检测测试
- RTT 计算准确性测试
- broadcast channel Lagged 记录 warn 日志并退出测试
- `majority_threshold` 对 1/2/3/5/7 节点的返回值验证

## 实现映射（关键函数/FFI）

- Membership 内部方法：`MembershipManager::add_peer`、`remove_peer`、`get_connected_peers`、`get_connected_peer_count`、`get_peer_rtt`、`get_peer_status`，这些方法在 `src/membership.rs` 实现并被 `NetworkState`/`transport` 的消息处理器调用。
- Leader 内部方法：`LeaderElection::set_leader`（返回 `(term, leader_id, assigner_id)` 并由调用方广播 `LeaderAssign`）、`LeaderElection::enable_auto_election`、`LeaderElection::leader_id()`、`LeaderElection::is_leader()`。FFI 对应函数为 `SetLeader`、`EnableAutoLeaderElection`、`GetCurrentLeader`、`IsLeader`（位于 `src/ffi.rs`）。
- 事件通知：`MembershipManager` 使用 `broadcast::Sender<MembershipEvent>`（容量 256），`LeaderElection` 通过 `leader_change_tx` 通知变化，FFI 的 `RegisterPeerStatusCallback` / `RegisterLeaderChangedCallback` 最终在 `callback.rs` 中触发 C# 回调。

（以上映射用于验证文档中关于事件流、投票统计与多数派阈值的描述与实现一致。）
