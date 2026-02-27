using System;
using System.Collections.Concurrent;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
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

  // --- 1. lifecycle ----------------------------------------------------

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

  // --- 2. local info queries -------------------------------------------

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

  // --- 3. messaging ----------------------------------------------------

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

  // --- 4. pre-init guards ----------------------------------------------

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

  // --- 5. error reporting ----------------------------------------------

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

  // --- 6. centralized-mode controls -----------------------------------

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

  // --- 7. leader APIs --------------------------------------------------

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

  // --- 8. connection result callback -----------------------------------

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

  // --- 9. config validation --------------------------------------------

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

  // --- 10. register callbacks before init returns error ---------------

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

  // --- 11. all four callbacks register successfully after init ---------

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

  // --- 12. EnableLogging smoke test ------------------------------------

  [Fact]
  public void EnableLoggingSmoke()
  {
    // EnableLogging is safe to call regardless of init state.
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.EnableLogging(0));
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.EnableLogging(1));
    Assert.Equal(NetErrorCode.OK, MetaMystiaNetwork.EnableLogging(1)); // idempotent
  }

  // --- 13. GetPeerList returns empty initially --------------------------

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

  // --- 14. GetPeerRTT / GetPeerStatus for non-existent peer ------------

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

  // --- 15. SendToPeer with unknown peer returns PeerNotFound ------------

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

  // --- 16. SendToLeader / SendFromLeader / ForwardMessage guard checks --

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

  // --- 17. SetLeader is reflected in GetCurrentLeader / IsLeader --------

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

  // --- 18. flags bit 0 is silently stripped, not rejected --------------

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

  // --- 19. DisconnectPeer on non-existent peer returns OK --------------

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

  // --- 20. NetworkConfigFFI struct size matches Rust #[repr(C)] --------

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
    // 56  max_connections              u32 → 4
    // 60  max_message_size             u32 → 4
    // 64  centralized_auto_forward     u8  → 1
    // 65  auto_election_enabled        u8  → 1
    // 66  mdns_port                    u16 → 2
    // 68  manual_override_recovery     u8  → 1
    // 69  [3 bytes alignment padding – next field is u64]
    // 72  handshake_timeout_ms         u64 → 8
    // total = 80 bytes
    //
    // C# LayoutKind.Sequential obeys the same rules and also produces 80 bytes.
    // If this test fails, a field type or order was changed in a way that breaks
    // the FFI ABI with the Rust side.
    Assert.Equal(80, Marshal.SizeOf<NetworkConfigFFI>());
  }

  // --- 21. ManualOverrideRecovery enum values ----------------------------

  [Fact]
  public void ManualOverrideRecoveryEnumValues()
  {
    // Verify the enum constants match the Rust side.
    Assert.Equal((byte)0, (byte)ManualOverrideRecovery.Hold);
    Assert.Equal((byte)1, (byte)ManualOverrideRecovery.AutoElect);
  }

  // --- 22. Config default includes manual_override_recovery = Hold -----

  [Fact]
  public void ConfigDefaultIncludesManualOverrideRecovery()
  {
    var cfg = NetworkConfigFFI.Default();
    Assert.Equal((byte)ManualOverrideRecovery.Hold, cfg.manual_override_recovery);
  }

  // --- 23. Init with ManualOverrideRecovery.AutoElect succeeds ---------

  [Fact]
  public void ConfigWithManualOverrideRecoveryAutoElect()
  {
    EnsureShutdown();

    var cfg = NetworkConfigFFI.Default();
    cfg.manual_override_recovery = (byte)ManualOverrideRecovery.AutoElect;
    MetaMystiaNetwork.Check(
        MetaMystiaNetwork.InitializeNetworkWithConfig("mor_peer", "session", ref cfg));

    MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
  }

  // --- 24. AlreadyConnected error code constant -------------------------

  [Fact]
  public void AlreadyConnectedErrorCodeValue()
  {
    Assert.Equal(-14, NetErrorCode.AlreadyConnected);
  }

  // --- 25. E2E: two-process multi-node connect and exchange messages ---

  [Fact]
  public void E2E_TwoNodeConnectAndExchangeMessages()
  {
    EnsureShutdown();

    // Locate PeerHelper project relative to test output directory
    string baseDir = AppContext.BaseDirectory;
    string helperProject = Path.GetFullPath(Path.Combine(
        baseDir, "..", "..", "..", "PeerHelper", "PeerHelper.csproj"));

    Assert.True(File.Exists(helperProject),
        $"PeerHelper project not found at {helperProject}");

    var psi = new ProcessStartInfo
    {
      FileName = "dotnet",
      Arguments = $"run --project \"{helperProject}\" -- helper_node e2e_csharp_session",
      RedirectStandardInput = true,
      RedirectStandardOutput = true,
      RedirectStandardError = true,
      UseShellExecute = false,
      CreateNoWindow = true,
    };

    using var helper = Process.Start(psi)!;
    var stdoutLines = new ConcurrentQueue<string>();

    // Background thread reads helper stdout into a queue
    var readerThread = new Thread(() =>
    {
      try
      {
        while (!helper.HasExited)
        {
          string? line = helper.StandardOutput.ReadLine();
          if (line != null) stdoutLines.Enqueue(line);
          else break;
        }
      }
      catch { }
    })
    { IsBackground = true };
    readerThread.Start();

    try
    {
      // 1. Wait for helper to report its listen address
      string helperAddr = WaitForStdoutLine(stdoutLines, "LISTEN:", 60_000);
      helperAddr = helperAddr.Replace("0.0.0.0", "127.0.0.1");

      // 2. Initialize our own network node
      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.InitializeNetwork("test_node", "e2e_csharp_session"));

      // 3. Set up callbacks
      var receivedMsgs = new ConcurrentQueue<(string peer, byte[] data, ushort msgType)>();

      _onRecv = (peerId, data, length, msgType, flags) =>
      {
        byte[] buf = new byte[length];
        Marshal.Copy(data, buf, 0, length);
        receivedMsgs.Enqueue((
            MetaMystiaNetwork.PtrToString(peerId), buf, msgType));
      };
      _onPeerStatus = OnPeerStatus;
      _onConn = (_, _, _) => { };

      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

      // 4. Connect to the helper node
      MetaMystiaNetwork.ConnectToPeer(helperAddr);

      // 5. Wait for connection to establish
      WaitForCondition(() => MetaMystiaNetwork.GetPeerCount() >= 1, 15_000,
          "connection to helper peer was not established");

      // 6. Send a user message to the helper
      byte[] payload = Encoding.UTF8.GetBytes("TEST_MSG_FROM_CSHARP");
      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.BroadcastMessage(payload, payload.Length, 0x0100, 0));

      // 7. Verify helper received our message (printed to its stdout)
      string recvLine = WaitForStdoutLine(stdoutLines, "RECV:", 10_000);
      Assert.False(string.IsNullOrEmpty(recvLine),
          "helper stdout RECV line was empty");

      // 8. Verify we received helper's greeting message
      WaitForCondition(() => !receivedMsgs.IsEmpty, 10_000,
          "did not receive greeting from helper node");

      Assert.True(receivedMsgs.TryDequeue(out var msg));
      Assert.Equal("helper_node", msg.peer);
      Assert.Equal(0x0100, msg.msgType);
      Assert.Equal("HELLO_FROM_HELPER", Encoding.UTF8.GetString(msg.data));

      MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
    }
    finally
    {
      try { helper.StandardInput.WriteLine("STOP"); } catch { }
      if (!helper.WaitForExit(5000))
        helper.Kill();
    }
  }

  // --- 26. E2E: three-node network via two helpers ---------------------

  [Fact]
  public void E2E_ThreeNodeMeshConnectivity()
  {
    EnsureShutdown();

    string helperProject = LocateHelperProject();

    using var helper1 = StartHelper(helperProject, "helper_1", "e2e_mesh_session");
    using var helper2 = StartHelper(helperProject, "helper_2", "e2e_mesh_session");
    var stdout1 = new ConcurrentQueue<string>();
    var stdout2 = new ConcurrentQueue<string>();
    StartStdoutReader(helper1, stdout1);
    StartStdoutReader(helper2, stdout2);

    try
    {
      string addr1 = WaitForStdoutLine(stdout1, "LISTEN:", 60_000).Replace("0.0.0.0", "127.0.0.1");
      string addr2 = WaitForStdoutLine(stdout2, "LISTEN:", 60_000).Replace("0.0.0.0", "127.0.0.1");

      // Initialize our test node
      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.InitializeNetwork("mesh_test", "e2e_mesh_session"));

      var receivedMsgs = new ConcurrentQueue<(string peer, byte[] data, ushort msgType)>();
      _onRecv = (peerId, data, length, msgType, flags) =>
      {
        byte[] buf = new byte[length];
        Marshal.Copy(data, buf, 0, length);
        receivedMsgs.Enqueue((MetaMystiaNetwork.PtrToString(peerId), buf, msgType));
      };
      _onPeerStatus = OnPeerStatus;
      _onConn = (_, _, _) => { };
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

      // Connect test node to both helpers
      MetaMystiaNetwork.ConnectToPeer(addr1);
      MetaMystiaNetwork.ConnectToPeer(addr2);

      WaitForCondition(() => MetaMystiaNetwork.GetPeerCount() >= 2, 15_000,
          "test node did not connect to both helpers");

      // Verify we get greeting messages from both helpers
      WaitForCondition(() =>
      {
        var msgs = receivedMsgs.ToArray();
        return msgs.Any(m => m.peer == "helper_1") && msgs.Any(m => m.peer == "helper_2");
      }, 15_000, "did not receive greetings from both helpers");

      // Send a broadcast that both helpers should receive
      byte[] payload = Encoding.UTF8.GetBytes("MESH_BROADCAST");
      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.BroadcastMessage(payload, payload.Length, 0x0200, 0));

      // Both helpers should print RECV: lines
      WaitForStdoutLine(stdout1, "RECV:", 10_000);
      WaitForStdoutLine(stdout2, "RECV:", 10_000);

      MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
    }
    finally
    {
      StopHelper(helper1);
      StopHelper(helper2);
    }
  }

  // --- 27. E2E: multiple messages round-trip ---------------------------

  [Fact]
  public void E2E_MultipleMessagesRoundTrip()
  {
    EnsureShutdown();

    string helperProject = LocateHelperProject();
    using var helper = StartHelper(helperProject, "multi_helper", "e2e_multi_session");
    var stdout = new ConcurrentQueue<string>();
    StartStdoutReader(helper, stdout);

    try
    {
      string helperAddr = WaitForStdoutLine(stdout, "LISTEN:", 60_000)
          .Replace("0.0.0.0", "127.0.0.1");

      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.InitializeNetwork("multi_test", "e2e_multi_session"));

      var receivedMsgs = new ConcurrentQueue<(string peer, byte[] data, ushort msgType)>();
      _onRecv = (peerId, data, length, msgType, flags) =>
      {
        byte[] buf = new byte[length];
        Marshal.Copy(data, buf, 0, length);
        receivedMsgs.Enqueue((MetaMystiaNetwork.PtrToString(peerId), buf, msgType));
      };
      _onPeerStatus = OnPeerStatus;
      _onConn = (_, _, _) => { };
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

      MetaMystiaNetwork.ConnectToPeer(helperAddr);
      WaitForCondition(() => MetaMystiaNetwork.GetPeerCount() >= 1, 15_000,
          "connection not established");

      // Wait for initial greeting
      WaitForCondition(() => !receivedMsgs.IsEmpty, 10_000,
          "did not receive greeting");

      // Send 10 numbered messages
      for (int i = 0; i < 10; i++)
      {
        byte[] payload = Encoding.UTF8.GetBytes($"MSG_{i}");
        MetaMystiaNetwork.Check(
            MetaMystiaNetwork.BroadcastMessage(payload, payload.Length, 0x0300, 0));
      }

      // Verify helper received all 10 (count RECV lines)
      int recvCount = 0;
      var sw = Stopwatch.StartNew();
      while (sw.ElapsedMilliseconds < 15_000 && recvCount < 10)
      {
        if (stdout.TryDequeue(out var line) && line.StartsWith("RECV:"))
          recvCount++;
        else
          Thread.Sleep(50);
      }
      Assert.True(recvCount >= 10,
          $"helper received only {recvCount}/10 messages");

      MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
    }
    finally
    {
      StopHelper(helper);
    }
  }

  // --- 28. E2E: large payload delivery (above compression threshold) ---

  [Fact]
  public void E2E_LargePayloadDelivery()
  {
    EnsureShutdown();

    string helperProject = LocateHelperProject();
    using var helper = StartHelper(helperProject, "large_helper", "e2e_large_session");
    var stdout = new ConcurrentQueue<string>();
    StartStdoutReader(helper, stdout);

    try
    {
      string helperAddr = WaitForStdoutLine(stdout, "LISTEN:", 60_000)
          .Replace("0.0.0.0", "127.0.0.1");

      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.InitializeNetwork("large_test", "e2e_large_session"));

      var receivedMsgs = new ConcurrentQueue<(string peer, byte[] data, ushort msgType)>();
      _onRecv = (peerId, data, length, msgType, flags) =>
      {
        byte[] buf = new byte[length];
        Marshal.Copy(data, buf, 0, length);
        receivedMsgs.Enqueue((MetaMystiaNetwork.PtrToString(peerId), buf, msgType));
      };
      _onPeerStatus = OnPeerStatus;
      _onConn = (_, _, _) => { };
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

      MetaMystiaNetwork.ConnectToPeer(helperAddr);
      WaitForCondition(() => MetaMystiaNetwork.GetPeerCount() >= 1, 15_000,
          "connection not established");

      // Build a 50 KB payload with recognizable pattern
      byte[] largePayload = new byte[50 * 1024];
      for (int i = 0; i < largePayload.Length; i++)
        largePayload[i] = (byte)(i % 251);

      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.BroadcastMessage(largePayload, largePayload.Length, 0x0400, 0));

      // Verify the helper received the large message
      string recvLine = WaitForStdoutLine(stdout, "RECV:", 15_000);
      Assert.False(string.IsNullOrEmpty(recvLine), "helper did not receive large payload");

      // Parse the base64 payload received by helper and verify size
      // Format: {peerId}:{msgType}:{base64}
      string[] parts = recvLine.Split(':', 3);
      Assert.True(parts.Length == 3, "malformed RECV line");
      byte[] receivedPayload = Convert.FromBase64String(parts[2]);
      Assert.Equal(largePayload.Length, receivedPayload.Length);
      Assert.Equal(largePayload, receivedPayload);

      MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
    }
    finally
    {
      StopHelper(helper);
    }
  }

  // --- 29. E2E: helper sends message via stdin command, test receives --

  [Fact]
  public void E2E_HelperSendsMessageOnCommand()
  {
    EnsureShutdown();

    string helperProject = LocateHelperProject();
    using var helper = StartHelper(helperProject, "cmd_helper", "e2e_cmd_session");
    var stdout = new ConcurrentQueue<string>();
    StartStdoutReader(helper, stdout);

    try
    {
      string helperAddr = WaitForStdoutLine(stdout, "LISTEN:", 60_000)
          .Replace("0.0.0.0", "127.0.0.1");

      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.InitializeNetwork("cmd_test", "e2e_cmd_session"));

      var receivedMsgs = new ConcurrentQueue<(string peer, byte[] data, ushort msgType)>();
      _onRecv = (peerId, data, length, msgType, flags) =>
      {
        byte[] buf = new byte[length];
        Marshal.Copy(data, buf, 0, length);
        receivedMsgs.Enqueue((MetaMystiaNetwork.PtrToString(peerId), buf, msgType));
      };
      _onPeerStatus = OnPeerStatus;
      _onConn = (_, _, _) => { };
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

      MetaMystiaNetwork.ConnectToPeer(helperAddr);
      WaitForCondition(() => MetaMystiaNetwork.GetPeerCount() >= 1, 15_000,
          "connection not established");

      // Drain helper's auto-greeting
      WaitForCondition(() => !receivedMsgs.IsEmpty, 10_000,
          "did not receive initial greeting");
      while (receivedMsgs.TryDequeue(out _)) { }

      // Command the helper to send us a specific message
      string customPayload = Convert.ToBase64String(Encoding.UTF8.GetBytes("COMMAND_MSG"));
      helper.StandardInput.WriteLine($"SEND:1024:{customPayload}");
      helper.StandardInput.Flush();

      // Verify we received the message
      WaitForCondition(() => !receivedMsgs.IsEmpty, 10_000,
          "did not receive commanded message from helper");

      Assert.True(receivedMsgs.TryDequeue(out var msg));
      Assert.Equal("cmd_helper", msg.peer);
      Assert.Equal(0x0400, msg.msgType); // 1024 = 0x0400
      Assert.Equal("COMMAND_MSG", Encoding.UTF8.GetString(msg.data));

      MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
    }
    finally
    {
      StopHelper(helper);
    }
  }

  // --- 30. E2E: two helpers connect to each other forming a triangle ---

  [Fact]
  public void E2E_TriangleTopology()
  {
    EnsureShutdown();

    string helperProject = LocateHelperProject();
    using var helper1 = StartHelper(helperProject, "tri_1", "e2e_tri_session");
    using var helper2 = StartHelper(helperProject, "tri_2", "e2e_tri_session");
    var stdout1 = new ConcurrentQueue<string>();
    var stdout2 = new ConcurrentQueue<string>();
    StartStdoutReader(helper1, stdout1);
    StartStdoutReader(helper2, stdout2);

    try
    {
      string addr1 = WaitForStdoutLine(stdout1, "LISTEN:", 60_000).Replace("0.0.0.0", "127.0.0.1");
      string addr2 = WaitForStdoutLine(stdout2, "LISTEN:", 60_000).Replace("0.0.0.0", "127.0.0.1");

      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.InitializeNetwork("tri_test", "e2e_tri_session"));

      _onRecv = (_, _, _, _, _) => { };
      _onPeerStatus = OnPeerStatus;
      _onConn = (_, _, _) => { };
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

      // test → helper1, test → helper2
      MetaMystiaNetwork.ConnectToPeer(addr1);
      MetaMystiaNetwork.ConnectToPeer(addr2);

      WaitForCondition(() => MetaMystiaNetwork.GetPeerCount() >= 2, 15_000,
          "test node did not connect to both helpers");

      // Instruct helper1 to connect to helper2 (forming triangle)
      helper1.StandardInput.WriteLine($"CONNECT:{addr2}");
      helper1.StandardInput.Flush();

      // Wait for helper1 to report helper2 as peer (or via CONNRESULT)
      WaitForStdoutLine(stdout1, "PEER:tri_2:", 15_000);

      // Query helper1's peer count – should see at least 2 (test + helper2)
      helper1.StandardInput.WriteLine("GETPEERCOUNT");
      helper1.StandardInput.Flush();
      string countStr = WaitForStdoutLine(stdout1, "PEERCOUNT:", 5_000);
      Assert.True(int.TryParse(countStr, out int h1Count));
      Assert.True(h1Count >= 2, $"helper1 peer count = {h1Count}, expected ≥ 2");

      MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
    }
    finally
    {
      StopHelper(helper1);
      StopHelper(helper2);
    }
  }

  // --- 31. E2E: unicast via SendToPeer to a specific helper ------------

  [Fact]
  public void E2E_UnicastViaSendToPeer()
  {
    EnsureShutdown();

    string helperProject = LocateHelperProject();
    using var helper = StartHelper(helperProject, "unihelper", "e2e_uni_session");
    var stdout = new ConcurrentQueue<string>();
    StartStdoutReader(helper, stdout);

    try
    {
      string helperAddr = WaitForStdoutLine(stdout, "LISTEN:", 60_000)
          .Replace("0.0.0.0", "127.0.0.1");

      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.InitializeNetwork("uni_test", "e2e_uni_session"));

      _onRecv = (_, _, _, _, _) => { };
      _onPeerStatus = OnPeerStatus;
      _onConn = (_, _, _) => { };
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

      MetaMystiaNetwork.ConnectToPeer(helperAddr);
      WaitForCondition(() => MetaMystiaNetwork.GetPeerCount() >= 1, 15_000,
          "connection not established");

      // Send a targeted (unicast) message to the helper using SendToPeer
      byte[] payload = Encoding.UTF8.GetBytes("UNICAST_TEST");
      int rc = MetaMystiaNetwork.SendToPeer("unihelper", payload, payload.Length, 0x0500, 0);
      MetaMystiaNetwork.Check(rc);

      // Helper should have received the message
      string recvLine = WaitForStdoutLine(stdout, "RECV:", 10_000);
      string[] parts = recvLine.Split(':', 3);
      Assert.True(parts.Length == 3, "malformed RECV line");
      Assert.Equal("uni_test", parts[0]);
      byte[] receivedPayload = Convert.FromBase64String(parts[2]);
      Assert.Equal("UNICAST_TEST", Encoding.UTF8.GetString(receivedPayload));

      MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
    }
    finally
    {
      StopHelper(helper);
    }
  }

  // --- 32. E2E: GetPeerList returns correct peer IDs -------------------

  [Fact]
  public void E2E_PeerListVerification()
  {
    EnsureShutdown();

    string helperProject = LocateHelperProject();
    using var helper1 = StartHelper(helperProject, "pv_h1", "e2e_pv_session");
    using var helper2 = StartHelper(helperProject, "pv_h2", "e2e_pv_session");
    var stdout1 = new ConcurrentQueue<string>();
    var stdout2 = new ConcurrentQueue<string>();
    StartStdoutReader(helper1, stdout1);
    StartStdoutReader(helper2, stdout2);

    try
    {
      string addr1 = WaitForStdoutLine(stdout1, "LISTEN:", 60_000).Replace("0.0.0.0", "127.0.0.1");
      string addr2 = WaitForStdoutLine(stdout2, "LISTEN:", 60_000).Replace("0.0.0.0", "127.0.0.1");

      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.InitializeNetwork("pv_test", "e2e_pv_session"));

      _onRecv = (_, _, _, _, _) => { };
      _onPeerStatus = OnPeerStatus;
      _onConn = (_, _, _) => { };
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

      MetaMystiaNetwork.ConnectToPeer(addr1);
      MetaMystiaNetwork.ConnectToPeer(addr2);

      WaitForCondition(() => MetaMystiaNetwork.GetPeerCount() >= 2, 15_000,
          "test node did not connect to both helpers");

      // GetPeerList should contain both helper peer IDs
      string peerListRaw = MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetPeerList());
      string[] peers = peerListRaw.Split('\n', StringSplitOptions.RemoveEmptyEntries);
      Assert.Contains("pv_h1", peers);
      Assert.Contains("pv_h2", peers);
      Assert.Equal(2, peers.Length);

      MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
    }
    finally
    {
      StopHelper(helper1);
      StopHelper(helper2);
    }
  }

  // --- 33. E2E: empty payload delivery via C# --------------------------

  [Fact]
  public void E2E_EmptyPayloadDelivery()
  {
    EnsureShutdown();

    string helperProject = LocateHelperProject();
    using var helper = StartHelper(helperProject, "emp_helper", "e2e_emp_session");
    var stdout = new ConcurrentQueue<string>();
    StartStdoutReader(helper, stdout);

    try
    {
      string helperAddr = WaitForStdoutLine(stdout, "LISTEN:", 60_000)
          .Replace("0.0.0.0", "127.0.0.1");

      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.InitializeNetwork("emp_test", "e2e_emp_session"));

      var receivedMsgs = new ConcurrentQueue<(string peer, byte[] data, ushort msgType)>();
      _onRecv = (peerId, data, length, msgType, flags) =>
      {
        byte[] buf = new byte[length];
        if (length > 0) Marshal.Copy(data, buf, 0, length);
        receivedMsgs.Enqueue((MetaMystiaNetwork.PtrToString(peerId), buf, msgType));
      };
      _onPeerStatus = OnPeerStatus;
      _onConn = (_, _, _) => { };
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterReceiveCallback(_onRecv));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn));

      MetaMystiaNetwork.ConnectToPeer(helperAddr);
      WaitForCondition(() => MetaMystiaNetwork.GetPeerCount() >= 1, 15_000,
          "connection not established");

      // Drain auto-greeting from helper
      WaitForCondition(() => !receivedMsgs.IsEmpty, 10_000,
          "did not receive initial greeting");
      while (receivedMsgs.TryDequeue(out _)) { }

      // Send an empty payload
      byte[] empty = Array.Empty<byte>();
      MetaMystiaNetwork.Check(
          MetaMystiaNetwork.BroadcastMessage(empty, 0, 0x0D00, 0));

      // Helper should receive it (RECV line with empty base64)
      string recvLine = WaitForStdoutLine(stdout, "RECV:", 10_000);
      string[] parts = recvLine.Split(':', 3);
      Assert.True(parts.Length == 3, "malformed RECV line");
      // Base64 of empty is ""
      byte[] receivedPayload = Convert.FromBase64String(parts[2]);
      Assert.Empty(receivedPayload);

      MetaMystiaNetwork.Check(MetaMystiaNetwork.ShutdownNetwork());
    }
    finally
    {
      StopHelper(helper);
    }
  }

  // --- E2E factored helpers --------------------------------------------

  static string LocateHelperProject()
  {
    string baseDir = AppContext.BaseDirectory;
    string path = Path.GetFullPath(Path.Combine(
        baseDir, "..", "..", "..", "PeerHelper", "PeerHelper.csproj"));
    Assert.True(File.Exists(path), $"PeerHelper project not found at {path}");
    return path;
  }

  static Process StartHelper(string project, string peerId, string sessionId)
  {
    var psi = new ProcessStartInfo
    {
      FileName = "dotnet",
      Arguments = $"run --project \"{project}\" -- {peerId} {sessionId}",
      RedirectStandardInput = true,
      RedirectStandardOutput = true,
      RedirectStandardError = true,
      UseShellExecute = false,
      CreateNoWindow = true,
    };
    return Process.Start(psi)!;
  }

  static void StartStdoutReader(Process proc, ConcurrentQueue<string> q)
  {
    var t = new Thread(() =>
    {
      try
      {
        while (!proc.HasExited)
        {
          string? line = proc.StandardOutput.ReadLine();
          if (line != null) q.Enqueue(line);
          else break;
        }
      }
      catch { }
    })
    { IsBackground = true };
    t.Start();
  }

  static void StopHelper(Process proc)
  {
    try { proc.StandardInput.WriteLine("STOP"); } catch { }
    if (!proc.WaitForExit(5000))
      proc.Kill();
  }

  // --- E2E helpers -----------------------------------------------------

  /// <summary>
  /// Blocks until a line starting with <paramref name="prefix"/> appears
  /// in the concurrent queue, returning the portion after the prefix.
  /// </summary>
  static string WaitForStdoutLine(
      ConcurrentQueue<string> lines, string prefix, int timeoutMs)
  {
    var sw = Stopwatch.StartNew();
    while (sw.ElapsedMilliseconds < timeoutMs)
    {
      if (lines.TryDequeue(out var line) && line.StartsWith(prefix))
        return line.Substring(prefix.Length);
      Thread.Sleep(50);
    }
    Assert.True(false, $"Timed out waiting for stdout line with prefix '{prefix}'");
    return string.Empty; // unreachable
  }

  /// <summary>
  /// Blocks until <paramref name="condition"/> returns true or the timeout
  /// expires, in which case the test fails with <paramref name="message"/>.
  /// </summary>
  static void WaitForCondition(Func<bool> condition, int timeoutMs, string message)
  {
    var sw = Stopwatch.StartNew();
    while (sw.ElapsedMilliseconds < timeoutMs)
    {
      if (condition()) return;
      Thread.Sleep(50);
    }
    Assert.True(false, message);
  }

  // --- helpers ---------------------------------------------------------

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
