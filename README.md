# meta-mystia-network

`meta-mystia-network` 是一个用 Rust 实现的轻量级 P2P 网络库，**通过标准 C ABI 暴露接口**，可以被 C/C++、C#、Python、Node.js 等多种语言直接调用。

原始动机来自于 [MetaMystia Mod](https://github.com/MetaMikuAI/MetaMystia) 的联机需求，但本库并不限于任何特定项目或语言，适合任何需要轻量级局域网内点对点通信的项目。

## 🌟 核心特性

- **TCP 全双工传输**，支持自动透明 LZ4 压缩
- **mDNS 局域网发现**，不可用时可直接指定 TCP 地址连接
- **会话隔离**：同一网段内通过 `session_id` 区分多个独立网络
- **Raft 选主**，适合多节点互联
- **可选的中心化路由模式**，广播消息由 Leader 代理转发
- **线程安全回调**：异步事件通过专用线程分发，不阻塞网络栈
- **运行时可调配置**：心跳间隔、选举超时、重连策略、压缩阈值均支持动态修改

## 🌍 语言绑定与调用

该库导出的所有接口都使用 `extern "C"` 和 `#[repr(C)]`，并在 `ffi.rs` 中定义错误码、配置结构及回调签名。

语言绑定示例：

- **C / C++**，直接在头文件中声明函数，链接动态库即可。
- **C# (.NET / Unity)**，使用 `DllImport`/`Marshal`。已有完整示例在 `csharp/` 目录。
- **Python**，通过 `ctypes` 或 `cffi` 调用 C ABI。
- **Node.js**，使用 `ffi-napi` 等 FFI 模块，或写 N‑API 插件。
- ...

以下章节包含具体示例和 API 参考。

## 🚀 快速上手

### 构建

```bash
cargo build --release
cargo build --release --features logging # 开启 logging 特性以输出内部日志到标准输出/错误
```

产物位于 `target/release/`：

| 平台    | 文件名                         |
| ------- | ------------------------------ |
| Windows | `meta_mystia_network.dll`      |
| macOS   | `libmeta_mystia_network.dylib` |

### 测试

```bash
cargo test
cd csharp && dotnet test
```

### C# 调用示例

```csharp
using System;
using System.Runtime.InteropServices;

static class MetaMystiaNetwork
{
  const string DLL = "meta_mystia_network";
  const CallingConvention CC = CallingConvention.Cdecl;
  const CharSet CS = CharSet.Ansi;

  [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
  public static extern int InitializeNetwork(string peerId, string sessionId);
  [DllImport(DLL, CallingConvention = CC)]
  public static extern int ShutdownNetwork();
  [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
  public static extern int ConnectToPeer(string addr);
  [DllImport(DLL, CallingConvention = CC)]
  public static extern int BroadcastMessage(byte[] data, int length, ushort msgType, byte flags);

  // 回调委托（示例）
  [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
  public delegate void ReceiveCallback(IntPtr peerId, IntPtr data, int length, ushort msgType, byte flags);
  [DllImport(DLL, CallingConvention = CC)]
  public static extern int RegisterReceiveCallback(ReceiveCallback callback);
}

// 初始化
MetaMystiaNetwork.InitializeNetwork("peer1", "sessionA");

// 注册接收回调
MetaMystiaNetwork.RegisterReceiveCallback((peerPtr, dataPtr, len, type, flags) =>
{
  string peer = Marshal.PtrToStringAnsi(peerPtr);
  byte[] buf = new byte[len];
  Marshal.Copy(dataPtr, buf, 0, len);
  Console.WriteLine($"recv from {peer}: type={type} len={len}");
});

// 连接到另一个节点并发送广播消息
MetaMystiaNetwork.ConnectToPeer("127.0.0.1:12345");
var payload = System.Text.Encoding.UTF8.GetBytes("hello");
MetaMystiaNetwork.BroadcastMessage(payload, payload.Length, 0x0100, 0);

// 清理
MetaMystiaNetwork.ShutdownNetwork();

// 更多 FFI 函数请参阅下方 API 参考或仓库中的 C# 示例与测试。
```

> `csharp/` 目录下有完整的 C# 示例与测试，可直接参考。回调实现与 FFI 细节见 `src/ffi.rs` 和 `csharp/NetworkTests.cs`。

## 📝 FFI 接口概览

以下列出所有对外导出的 `extern "C"` 函数及其行为，`csharp/NetworkTests.cs` 中有完整测试覆盖。

| 函数                                                  | 描述                                 | 备注                                                                      |
| ----------------------------------------------------- | ------------------------------------ | ------------------------------------------------------------------------- |
| `InitializeNetwork(peerId, sessionId)`                | 初始化网络状态，必须在首次使用前调用 | 重复调用返回 `AlreadyInitialized`；可通过 `IsNetworkInitialized` 检查     |
| `InitializeNetworkWithConfig(peerId, sessionId, cfg)` | 使用自定义配置初始化                 | `cfg` 不能为空指针；结构体大小须为 64 字节且字段顺序与 Rust 端对齐        |
| `ShutdownNetwork()`                                   | 关闭并清理所有资源                   | 未初始化时调用返回 `NotInitialized`；关闭后可重新调用 `InitializeNetwork` |
| `IsNetworkInitialized()`                              | 已初始化返回 `1`，否则返回 `0`       |                                                                           |
| `GetLastErrorCode()` / `GetLastErrorMessage()`        | 获取最近一次调用失败的错误码和描述   | 消息字符串有效期至下一次字符串返回前                                      |

### 连接管理

| 函数                     | 功能                            | 备注                                                             |
| ------------------------ | ------------------------------- | ---------------------------------------------------------------- |
| `ConnectToPeer(addr)`    | 提交异步连接请求到指定 TCP 地址 | 仅在已初始化时有效；连接结果通过 `ConnectionResultCallback` 通知 |
| `DisconnectPeer(peerId)` | 断开指定 peer 并从列表移除      | 未知 peer ID 也返回 `OK`                                         |

### 本地状态查询

| 函数                                                     | 功能                 | 备注                |
| -------------------------------------------------------- | -------------------- | ------------------- |
| `GetLocalPeerId()` / `GetSessionId()` / `GetLocalAddr()` | 返回字符串指针       | 有效期至下次调用    |
| `GetPeerCount()` / `GetPeerList()`                       | 节点数或换行分隔列表 | 未连接时为空        |
| `GetPeerRTT(peerId)` / `GetPeerStatus(peerId)`           | RTT / 状态           | 未知 peer 返回 `-1` |

### 领导与模式控制

| 函数                                                               | 功能                                     | 备注                                                          |
| ------------------------------------------------------------------ | ---------------------------------------- | ------------------------------------------------------------- |
| `SetLeader(peerId)`                                                | 手动指定 Leader                          | 广播 `LeaderAssign` 通知所有节点                              |
| `EnableAutoLeaderElection(enable)`                                 | 启/关自动选主                            | 默认开启                                                      |
| `GetCurrentLeader()` / `IsLeader()`                                | 查询当前 Leader ID / 本节点是否为 Leader |                                                               |
| `SetCentralizedMode(enable)` / `IsCentralizedMode()`               | 启/关集中转发模式                        | 默认禁用（`0`）                                               |
| `SetCentralizedAutoForward(enable)` / `IsCentralizedAutoForward()` | 控制 Leader 是否自动转发广播             | 默认开启（`1`）                                               |
| `SetCompressionThreshold(thresh)`                                  | 调整 LZ4 压缩阈值（字节）                | payload 超过此值时尝试压缩；设为 `0` 时所有非空消息均尝试压缩 |

### 消息发送

| 函数                                               | 功能                         | 备注                                                       |
| -------------------------------------------------- | ---------------------------- | ---------------------------------------------------------- |
| `BroadcastMessage(data, len, type, flags)`         | 向所有已连接节点广播         | `type >= 0x0100`；`flags` 最低位由库内部使用，会被自动清零 |
| `SendToPeer(peer, data, len, type, flags)`         | 向指定 peer 发送消息         | peer 不存在返回 `PeerNotFound`                             |
| `SendToLeader(data, len, type, flags)`             | 将消息发送给当前 Leader      | 无 Leader 时返回 `NotLeader`                               |
| `SendFromLeader(data, len, type, flags)`           | 以 Leader 身份向所有节点广播 | 本节点非 Leader 时返回 `NotLeader`                         |
| `ForwardMessage(from, to, data, len, type, flags)` | Leader 代理转发消息          | `to==NULL` 时广播给所有节点；空字符串视为具体 peer ID      |

### 回调注册

| 函数                                   | 功能                     | 备注                                                                        |
| -------------------------------------- | ------------------------ | --------------------------------------------------------------------------- |
| `RegisterReceiveCallback(cb)`          | 注册用户消息到达回调     | 必须在 `InitializeNetwork` 成功后调用，且需在整个生命周期内保持委托引用存活 |
| `RegisterLeaderChangedCallback(cb)`    | 注册 Leader 变更通知回调 |                                                                             |
| `RegisterPeerStatusCallback(cb)`       | 注册节点状态变更回调     |                                                                             |
| `RegisterConnectionResultCallback(cb)` | 注册异步连接结果回调     |                                                                             |

### 工具函数

| 函数                    | 功能                  | 备注                           |
| ----------------------- | --------------------- | ------------------------------ |
| `EnableLogging(enable)` | 开关 tracing 日志输出 | 需以 `--features logging` 编译 |

> 所有函数以 `int` 返回错误码，`0`（`OK`）表示成功。完整错误码定义见 `src/error.rs` 或 C# 的 `NetErrorCode`。

## 🧩 配置与默认值

默认采用 `NetworkConfig::default()`，对应 C# 的 `NetworkConfigFFI.Default()`，一般无需修改。`NetworkConfigFFI` 的内存布局必须与 Rust 端完全一致，结构体总大小为 **64 字节**：

```c
// 总大小 64 字节（含编译器隐式对齐填充，详见 #[repr(C)] 布局）
struct NetworkConfigFFI {
    uint64_t heartbeat_interval_ms;        // 默认 500
    uint64_t election_timeout_min_ms;      // 默认 1500
    uint64_t election_timeout_max_ms;      // 默认 3000
    uint32_t heartbeat_timeout_multiplier; // 默认 3
    uint8_t  _implicit_padding[4];         // 编译器自动插入，保证下一个 uint64_t 8 字节对齐

    uint64_t reconnect_initial_ms;         // 默认 1000
    uint64_t reconnect_max_ms;             // 默认 30000

    uint32_t compression_threshold;        // 默认 512（字节）
    uint32_t send_queue_capacity;          // 默认 1024

    uint8_t  centralized_auto_forward;     // 默认 1
    uint8_t  auto_election_enabled;        // 默认 1
    uint8_t  _padding[2];                  // 显式填充，保证结构体末尾 4 字节对齐
};
```

- `validate()` 会校验字段合理性（非零、范围关系等），校验失败时 `InitializeNetworkWithConfig` 返回 `InvalidArgument`。
- C# 侧使用 `LayoutKind.Sequential` 且不指定 `Pack` 时，编译器将自动处理隐式对齐填充，无需手动插入。
- C# 绑定提供 `NetworkConfigFFI.Default()`，单元测试已校验结构体大小为 64 字节。

> ⚠️ 修改 `NetworkConfigFFI` 的字段顺序或类型会破坏跨语言 ABI 兼容性，导致内存损坏。

## 📫 回调与事件

所有异步事件均通过独立回调线程分发，库内部保证线程安全。调用方必须在整个网络生命周期内持有回调对象引用，否则会产生悬空指针。主要签名如下：

```c
// 消息到达
void (*ReceiveCallback)(const char *peerId, const uint8_t *data, int length, uint16_t msgType, uint8_t flags);

// 领导者变化
void (*LeaderChangedCallback)(const char *leaderPeerId);

// 节点状态更新
void (*PeerStatusCallback)(const char *peerId, int status);

// 异步连接结果
void (*ConnectionResultCallback)(const char *addr, uint8_t success, int errorCode);
```

- `PeerStatus` 整数映射：`Connected=0`、`Disconnected=1`、`Reconnecting=2`、`Handshaking=3`
- C# 中须将委托保存为静态字段或成员变量，防止被 GC 回收后出现悬空指针。
- 所有 `Register*Callback` 函数必须在 `InitializeNetwork` 成功后调用，否则返回 `NotInitialized`。

## 📚 错误处理

- 所有函数以 `int` 返回错误码。`0`（`OK`）表示成功，负数表示具体错误，常见值有 `NotInitialized (-1)`、`AlreadyInitialized (-2)`、`InvalidArgument (-3)`、`PeerNotFound (-5)`、`NotLeader (-6)` 等。
- 失败后可通过 `GetLastErrorCode()` / `GetLastErrorMessage()` 获取详细信息，消息字符串有效期至下一次返回字符串的调用前。
- 在 `InitializeNetwork` 成功之前调用绝大多数 API 均会返回 `NotInitialized`（见测试 `ApiCallsBeforeInitReturnNotInitialized`）。

## 📄 设计文档

详细设计文档位于 `.github/prompts/` 目录，涵盖以下主题：

- 模块总览与架构设计
- 核心类型与通信协议
- 传输层设计
- 成员管理与 Raft 选主
- 消息路由与 mDNS 发现
- FFI 导出与回调机制
- 测试计划
- C# 接入指南
