using System;
using System.Collections.Concurrent;
using System.Runtime.InteropServices;
using System.Threading;
using MetaMystiaNetworkBindings;
using Xunit;

// All tests manipulate the same process-wide FFI singleton, so they
// must not run in parallel.

[Collection("NetworkSerial")]
public class NetworkTests
{
  // Keep delegate roots alive for the lifetime of the test process –
  // the native side holds raw function pointers.
  // Declared nullable because they are assigned per-test, not at field initialization.
  static ReceiveCallback? _onRecv;
  static LeaderChangedCallback? _onLeaderChanged;
  static PeerStatusCallback? _onPeerStatus;
  static ConnectionResultCallback? _onConn;
  static readonly ConcurrentQueue<NetworkEvent> _queue = new();

  /// Best-effort teardown between tests: ignore return code so that
  /// each test always starts from a clean state.
  void EnsureShutdown() => MetaMystiaNetwork.ShutdownNetwork();

  // ── 1. lifecycle ─────────────────────────────────────────────────────

  [Fact]
  public void InitShutdownCycle()
  {
    EnsureShutdown();

    int r = MetaMystiaNetwork.InitializeNetwork("csharp_test1", "session");
    MetaMystiaNetwork.Check(r);
    Assert.Equal((byte)1, MetaMystiaNetwork.IsNetworkInitialized());

    // callbacks must be registered AFTER the network is initialized
    _onRecv = OnReceive;
    int reg = MetaMystiaNetwork.RegisterReceiveCallback(_onRecv);
    Assert.Equal(NetErrorCode.OK, reg);

    r = MetaMystiaNetwork.ShutdownNetwork();
    MetaMystiaNetwork.Check(r);
    Assert.Equal((byte)0, MetaMystiaNetwork.IsNetworkInitialized());
  }

  [Fact]
  public void DoubleInitReturnsAlreadyInitialized()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("dup_peer", "session"));
    int rc = MetaMystiaNetwork.InitializeNetwork("dup_peer2", "session");
    Assert.Equal(NetErrorCode.AlreadyInitialized, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 2. local info queries ────────────────────────────────────────────

  [Fact]
  public void GetLocalInfoAfterInit()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("info_peer", "info_session"));

    string peerId = MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetLocalPeerId());
    Assert.Equal("info_peer", peerId);

    string sessionId = MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetSessionId());
    Assert.Equal("info_session", sessionId);

    string addr = MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetLocalAddr());
    Assert.False(string.IsNullOrEmpty(addr), "local addr must not be empty");
    Assert.Contains(":", addr); // expected "host:port" format

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  [Fact]
  public void GetPeerCountIsZeroInitially()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("count_peer", "session"));
    Assert.Equal(0, MetaMystiaNetwork.GetPeerCount());

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 3. messaging ─────────────────────────────────────────────────────

  [Fact]
  public void BroadcastWithoutPeersDoesNotCrash()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("csharp_test2", "session"));
    // No connected peers – message is silently dropped; must return OK.
    int rc = MetaMystiaNetwork.BroadcastMessage(new byte[] { 1, 2, 3 }, 3, 0x0100, 0);
    Assert.Equal(NetErrorCode.OK, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  [Fact]
  public void InternalMsgTypeRejected()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("type_peer", "session"));

    // 0x0001–0x00FF are reserved for internal protocol use
    int rc = MetaMystiaNetwork.BroadcastMessage(new byte[] { 1 }, 1, 0x00FF, 0);
    Assert.Equal(NetErrorCode.InvalidArgument, rc);

    rc = MetaMystiaNetwork.BroadcastMessage(new byte[] { 1 }, 1, 0x0001, 0);
    Assert.Equal(NetErrorCode.InvalidArgument, rc);

    // 0x0100 is the minimum valid user msg_type
    rc = MetaMystiaNetwork.BroadcastMessage(new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.OK, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 4. pre-init guards ───────────────────────────────────────────────

  [Fact]
  public void ApiCallsBeforeInitReturnNotInitialized()
  {
    EnsureShutdown();

    int rc = MetaMystiaNetwork.ConnectToPeer("127.0.0.1:12345");
    Assert.Equal(NetErrorCode.NotInitialized, rc);

    rc = MetaMystiaNetwork.SetLeader("foo");
    Assert.Equal(NetErrorCode.NotInitialized, rc);

    rc = MetaMystiaNetwork.BroadcastMessage(new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.NotInitialized, rc);

    rc = MetaMystiaNetwork.DisconnectPeer("foo");
    Assert.Equal(NetErrorCode.NotInitialized, rc);
  }

  // ── 5. error reporting ───────────────────────────────────────────────

  [Fact]
  public void LastErrorUpdatedOnFailure()
  {
    EnsureShutdown();

    // Trigger a known failure
    int rc = MetaMystiaNetwork.BroadcastMessage(new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.NotInitialized, rc);

    // GetLastErrorCode must reflect the most recent failure
    Assert.Equal(NetErrorCode.NotInitialized, MetaMystiaNetwork.GetLastErrorCode());

    // GetLastErrorMessage must be a non-empty human-readable description
    string msg = MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetLastErrorMessage());
    Assert.False(string.IsNullOrEmpty(msg), "error message must not be empty");
  }

  // ── 6. centralized-mode controls ────────────────────────────────────

  [Fact]
  public void CentralizedModeToggle()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("central_peer", "session"));

    // default: disabled
    Assert.Equal((byte)0, MetaMystiaNetwork.IsCentralizedMode());

    MetaMystiaNetwork.Check(MetaMystiaNetwork.SetCentralizedMode(1));
    Assert.Equal((byte)1, MetaMystiaNetwork.IsCentralizedMode());

    MetaMystiaNetwork.Check(MetaMystiaNetwork.SetCentralizedMode(0));
    Assert.Equal((byte)0, MetaMystiaNetwork.IsCentralizedMode());

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  [Fact]
  public void CentralizedAutoForwardToggle()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("af_peer", "session"));

    // default from NetworkConfigFFI.Default(): enabled (1)
    Assert.Equal((byte)1, MetaMystiaNetwork.IsCentralizedAutoForward());

    MetaMystiaNetwork.Check(MetaMystiaNetwork.SetCentralizedAutoForward(0));
    Assert.Equal((byte)0, MetaMystiaNetwork.IsCentralizedAutoForward());

    MetaMystiaNetwork.Check(MetaMystiaNetwork.SetCentralizedAutoForward(1));
    Assert.Equal((byte)1, MetaMystiaNetwork.IsCentralizedAutoForward());

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  [Fact]
  public void CompressionThresholdSetting()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("comp_peer", "session"));

    MetaMystiaNetwork.Check(MetaMystiaNetwork.SetCompressionThreshold(1024));
    MetaMystiaNetwork.Check(MetaMystiaNetwork.SetCompressionThreshold(0)); // disabled

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 7. leader APIs ───────────────────────────────────────────────────

  [Fact]
  public void LeaderApisDoNotCrash()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("solo_peer", "session"));

    // EnableAutoLeaderElection must succeed in both directions
    MetaMystiaNetwork.Check(MetaMystiaNetwork.EnableAutoLeaderElection(0));
    MetaMystiaNetwork.Check(MetaMystiaNetwork.EnableAutoLeaderElection(1));

    // Query APIs must not crash regardless of election state
    _ = MetaMystiaNetwork.IsLeader();
    _ = MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetCurrentLeader()); // may be empty

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 8. connection result callback ────────────────────────────────────

  [Fact]
  public void ConnectionResultCallbackFiresOnBadAddress()
  {
    EnsureShutdown();

    var mre = new ManualResetEventSlim(false);
    int lastCode = 0;
    byte lastSuccess = 255; // sentinel – must be overwritten by the callback

    _onConn = (addr, success, code) =>
    {
      lastSuccess = success;
      lastCode = code;
      mre.Set();
    };

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("csharp_test3", "session"));

    _onRecv = OnReceive; // keep delegate alive
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

    // TEST-NET-1 (192.0.2.0/24) is non-routable; connection must fail
    MetaMystiaNetwork.ConnectToPeer("192.0.2.1:12345");
    Assert.True(mre.Wait(10_000), "connection-result callback must fire within 10 s");

    Assert.Equal((byte)0, lastSuccess);
    Assert.NotEqual(NetErrorCode.OK, lastCode);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 9. config validation ─────────────────────────────────────────────

  [Fact]
  public void ConfigInitializationValidation()
  {
    EnsureShutdown();

    // send_queue_capacity == 0  →  InvalidArgument
    var cfg = NetworkConfigFFI.Default();
    cfg.send_queue_capacity = 0;
    int rc = MetaMystiaNetwork.InitializeNetworkWithConfig("a", "b", ref cfg);
    Assert.Equal(NetErrorCode.InvalidArgument, rc);

    // heartbeat_interval_ms == 0  →  InvalidArgument
    cfg = NetworkConfigFFI.Default();
    cfg.heartbeat_interval_ms = 0;
    rc = MetaMystiaNetwork.InitializeNetworkWithConfig("a", "b", ref cfg);
    Assert.Equal(NetErrorCode.InvalidArgument, rc);

    // election_timeout_min > election_timeout_max  →  InvalidArgument
    cfg = NetworkConfigFFI.Default();
    cfg.election_timeout_min_ms = 5000;
    cfg.election_timeout_max_ms = 1000;
    rc = MetaMystiaNetwork.InitializeNetworkWithConfig("a", "b", ref cfg);
    Assert.Equal(NetErrorCode.InvalidArgument, rc);

    // valid default config must succeed
    cfg = NetworkConfigFFI.Default();
    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetworkWithConfig("a", "b", ref cfg));
    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 10. register callbacks before init returns error ────────────────

  [Fact]
  public void RegisterCallbacksBeforeInitReturnNotInitialized()
  {
    EnsureShutdown();

    // All Register* require the network to be initialized first.
    // They route through with_network() which returns NotInitialized.
    _onRecv = OnReceive;
    Assert.Equal(NetErrorCode.NotInitialized, MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));

    _onLeaderChanged = OnLeaderChanged;
    Assert.Equal(NetErrorCode.NotInitialized, MetaMystiaNetwork.RegisterLeaderChangedCallback(_onLeaderChanged));

    _onPeerStatus = OnPeerStatus;
    Assert.Equal(NetErrorCode.NotInitialized, MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));

    _onConn = (_, _, _) => { };
    Assert.Equal(NetErrorCode.NotInitialized, MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));
  }

  // ── 11. all four callbacks register successfully after init ──────────

  [Fact]
  public void RegisterAllCallbacksAfterInit()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("reg_peer", "session"));

    _onRecv = OnReceive;
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));

    _onLeaderChanged = OnLeaderChanged;
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.RegisterLeaderChangedCallback(_onLeaderChanged));

    _onPeerStatus = OnPeerStatus;
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));

    _onConn = (_, _, _) => { };
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 12. EnableLogging smoke test ─────────────────────────────────────

  [Fact]
  public void EnableLoggingSmoke()
  {
    // EnableLogging is safe to call regardless of init state.
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.EnableLogging(0));
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.EnableLogging(1));
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.EnableLogging(1)); // idempotent
  }

  // ── 13. GetPeerList returns empty initially ───────────────────────────

  [Fact]
  public void GetPeerListEmptyInitially()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("list_peer", "session"));

    // Must not return null; empty or a single empty string are both acceptable.
    string raw = MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetPeerList());
    Assert.NotNull(raw);

    // Splitting as the doc example shows must yield 0 active peers.
    string[] peers = raw.Split('\n', StringSplitOptions.RemoveEmptyEntries);
    Assert.Empty(peers);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 14. GetPeerRTT / GetPeerStatus for non-existent peer ─────────────

  [Fact]
  public void PeerQueryApisReturnSentinelForUnknownPeer()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("query_peer", "session"));

    // Non-existent peer: RTT must be -1
    Assert.Equal(-1, MetaMystiaNetwork.GetPeerRTT("nobody"));

    // Non-existent peer: status must be -1 (no such peer)
    Assert.Equal(-1, MetaMystiaNetwork.GetPeerStatus("nobody"));

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 15. SendToPeer with unknown peer returns PeerNotFound ─────────────

  [Fact]
  public void SendToPeerUnknownPeerReturnsNotFound()
  {
    EnsureShutdown();

    // Pre-init guard
    int rc = MetaMystiaNetwork.SendToPeer("nobody", new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.NotInitialized, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("send_peer", "session"));

    // Connected but peer doesn't exist → PeerNotFound
    rc = MetaMystiaNetwork.SendToPeer("nobody", new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.PeerNotFound, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 16. SendToLeader / SendFromLeader / ForwardMessage guard checks ───

  [Fact]
  public void SendToLeaderReturnsNotLeaderWhenNoLeader()
  {
    EnsureShutdown();

    // Pre-init guard
    int rc = MetaMystiaNetwork.SendToLeader(new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.NotInitialized, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("sl_peer", "session"));

    // No leader has been elected; SendToLeader must fail with NotLeader.
    rc = MetaMystiaNetwork.SendToLeader(new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.NotLeader, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  [Fact]
  public void SendFromLeaderReturnsNotLeader()
  {
    EnsureShutdown();

    // Pre-init guard
    int rc = MetaMystiaNetwork.SendFromLeader(new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.NotInitialized, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("sfl_peer", "session"));

    // This node is NOT the leader; the call must be rejected.
    rc = MetaMystiaNetwork.SendFromLeader(new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.NotLeader, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  [Fact]
  public void ForwardMessageReturnsNotLeader()
  {
    EnsureShutdown();

    // Pre-init guard
    int rc = MetaMystiaNetwork.ForwardMessage("from", "to", new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.NotInitialized, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("fwd_peer", "session"));

    // This node is NOT the leader; ForwardMessage must reject.
    rc = MetaMystiaNetwork.ForwardMessage("from", "to", new byte[] { 1 }, 1, 0x0100, 0);
    Assert.Equal(NetErrorCode.NotLeader, rc);

    // Broadcast variant (null target handled by Rust side):
    // Rust ForwardMessage: target=null → ForwardTarget::Broadcast, but still requires leader.
    // We cannot pass C# null for string easily via P/Invoke here; the non-null path is sufficient.

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 17. SetLeader is reflected in GetCurrentLeader / IsLeader ─────────

  [Fact]
  public void SetLeaderReflectedInQueries()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("host_peer", "session"));

    // Before any election or manual assignment the leader may be empty.
    // Manually assign a leader (peer does not need to be connected).
    MetaMystiaNetwork.Check(MetaMystiaNetwork.SetLeader("remote_peer"));

    string leader = MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetCurrentLeader());
    Assert.Equal("remote_peer", leader);

    // Our local peer is "host_peer", not "remote_peer" → not leader.
    Assert.Equal((byte)0, MetaMystiaNetwork.IsLeader());

    // Now assign ourselves as leader.
    MetaMystiaNetwork.Check(MetaMystiaNetwork.SetLeader("host_peer"));
    Assert.Equal((byte)1, MetaMystiaNetwork.IsLeader());

    string leaderSelf = MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetCurrentLeader());
    Assert.Equal("host_peer", leaderSelf);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 18. flags bit 0 is silently stripped, not rejected ───────────────

  [Fact]
  public void FlagsBit0IsStrippedNotRejected()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("flags_peer", "session"));

    // Passing flags=0xFF (bit 0 set by caller) must return OK –
    // the library masks bit 0 internally; it is not a protocol error.
    int rc = MetaMystiaNetwork.BroadcastMessage(new byte[] { 1, 2, 3 }, 3, 0x0100, 0xFF);
    Assert.Equal(NetErrorCode.OK, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 19. DisconnectPeer on non-existent peer returns OK ───────────────

  [Fact]
  public void DisconnectNonExistentPeerReturnsOk()
  {
    EnsureShutdown();

    MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork("disc_peer", "session"));

    // The transport silently ignores disconnects for unknown peers.
    int rc = MetaMystiaNetwork.DisconnectPeer("nobody");
    Assert.Equal(NetErrorCode.OK, rc);

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // ── 20. NetworkConfigFFI struct size matches Rust #[repr(C)] ─────────

  [Fact]
  public void NetworkConfigFFISizeMatchesRustLayout()
  {
    // Rust layout (bytes):
    //  0  heartbeat_interval_ms        u64 → 8
    //  8  election_timeout_min_ms      u64 → 8
    // 16  election_timeout_max_ms      u64 → 8
    // 24  heartbeat_timeout_multiplier u32 → 4
    // 28  [4 bytes alignment padding – next field is u64]
    // 32  reconnect_initial_ms         u64 → 8
    // 40  reconnect_max_ms             u64 → 8
    // 48  compression_threshold        u32 → 4
    // 52  send_queue_capacity          u32 → 4
    // 56  centralized_auto_forward     u8  → 1
    // 57  auto_election_enabled        u8  → 1
    // 58  _padding                     u8×2→ 2
    // 60  [4 bytes trailing padding – struct rounded to largest member alignment = 8]
    // total = 64 bytes
    //
    // C# LayoutKind.Sequential obeys the same rules and also produces 64 bytes.
    // If this test fails, a field type or order was changed in a way that breaks
    // the FFI ABI with the Rust side.
    Assert.Equal(64, Marshal.SizeOf<NetworkConfigFFI>());
  }

  // ── helpers ──────────────────────────────────────────────────────────

  static void OnReceive(IntPtr peerId, IntPtr data, int length, ushort msgType, byte flags)
  {
    byte[] buf = new byte[length];
    Marshal.Copy(data, buf, 0, length);
    _queue.Enqueue(new NetworkEvent
    {
      Type = NetworkEventType.Message,
      PeerId = MetaMystiaNetwork.PtrToString(peerId),
      MsgType = msgType,
      Data = buf
    });
  }

  static void OnLeaderChanged(IntPtr leaderPeerId)
  {
    _queue.Enqueue(new NetworkEvent
    {
      Type = NetworkEventType.LeaderChanged,
      PeerId = MetaMystiaNetwork.PtrToString(leaderPeerId),
    });
  }

  static void OnPeerStatus(IntPtr peerId, int status)
  {
    _queue.Enqueue(new NetworkEvent
    {
      Type = NetworkEventType.PeerStatus,
      PeerId = MetaMystiaNetwork.PtrToString(peerId),
      Status = (PeerStatus)status,
    });
  }
}

// Prevents xunit from running tests in different classes of this
// collection in parallel – all NetworkTests share a single FFI singleton.
[CollectionDefinition("NetworkSerial", DisableParallelization = true)]
public class NetworkSerialCollection { }
