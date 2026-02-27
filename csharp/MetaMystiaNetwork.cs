using System;
using System.Collections.Concurrent;
using System.Runtime.InteropServices;

namespace MetaMystiaNetworkBindings
{
  // --- Error codes -----------------------------------------------------

  /// <summary>
  /// Numeric error codes returned by every FFI function that returns <c>int</c>.
  /// Zero means success; negative values indicate specific failure modes.
  /// </summary>
  public static class NetErrorCode
  {
    /// <summary>Operation completed successfully.</summary>
    public const int OK = 0;

    /// <summary>Network has not been initialized; call <see cref="MetaMystiaNetwork.InitializeNetwork"/> first.</summary>
    public const int NotInitialized = -1;

    /// <summary>
    /// Network is already initialized.
    /// Call <see cref="MetaMystiaNetwork.ShutdownNetwork"/> before re-initializing.
    /// </summary>
    public const int AlreadyInitialized = -2;

    /// <summary>One or more arguments failed validation (e.g. invalid config, reserved msg_type).</summary>
    public const int InvalidArgument = -3;

    /// <summary>TCP connection to the remote peer could not be established.</summary>
    public const int ConnectionFailed = -4;

    /// <summary>The specified peer ID is not currently known or connected.</summary>
    public const int PeerNotFound = -5;

    /// <summary>The operation requires this node to be the current Leader.</summary>
    public const int NotLeader = -6;

    /// <summary>The per-peer outgoing send queue is full; the message was discarded.</summary>
    public const int SendQueueFull = -7;

    /// <summary>Message payload exceeds the 1 MB hard limit.</summary>
    public const int MessageTooLarge = -8;

    /// <summary>Internal serialization / deserialization failure (postcard).</summary>
    public const int SerializationError = -9;

    /// <summary>Remote peer belongs to a different session_id and was rejected.</summary>
    public const int SessionMismatch = -10;

    /// <summary>A peer with the same ID is already connected.</summary>
    public const int DuplicatePeerId = -11;

    /// <summary>Remote peer advertises an incompatible protocol version.</summary>
    public const int VersionMismatch = -12;

    /// <summary>The connection limit (64) has been reached; new connections are refused.</summary>
    public const int MaxConnectionsReached = -13;

    /// <summary>
    /// The peer is already connected (not an error). ConnectToPeer treats this
    /// as success with this code so the caller knows no new connection was created.
    /// </summary>
    public const int AlreadyConnected = -14;

    /// <summary>Unexpected internal error inside the Rust library; check logs for details.</summary>
    public const int InternalError = -99;
  }

  // --- Enumerations ----------------------------------------------------

  /// <summary>
  /// Controls what happens when a manually-assigned leader goes offline.
  /// Used only when <c>manual_override</c> is active in the election module.
  /// </summary>
  public enum ManualOverrideRecovery : byte
  {
    /// <summary>
    /// Keep manual override active; do NOT start an automatic election.
    /// The upper layer is expected to call <see cref="MetaMystiaNetwork.SetLeader"/>
    /// again or take other recovery action.
    /// </summary>
    Hold = 0,

    /// <summary>
    /// Clear manual override and let the normal Raft-like election take over,
    /// choosing a new leader automatically.
    /// </summary>
    AutoElect = 1,
  }

  /// <summary>
  /// Connection state of a remote peer as reported by
  /// <see cref="MetaMystiaNetwork.GetPeerStatus"/> and via <see cref="PeerStatusCallback"/>.
  /// </summary>
  public enum PeerStatus
  {
    /// <summary>Fully connected and heartbeat is alive.</summary>
    Connected = 0,

    /// <summary>Connection lost or cleanly closed.</summary>
    Disconnected = 1,

    /// <summary>Attempting to reconnect with exponential back-off.</summary>
    Reconnecting = 2,

    /// <summary>TCP stream connected but protocol handshake not yet complete.</summary>
    Handshaking = 3,
  }

  /// <summary>Discriminates which kind of event is stored in a <see cref="NetworkEvent"/>.</summary>
  public enum NetworkEventType
  {
    /// <summary>A user message was received from a remote peer.</summary>
    Message,

    /// <summary>A peer's connection state changed.</summary>
    PeerStatus,

    /// <summary>The Leader peer changed (or became absent).</summary>
    LeaderChanged,

    /// <summary>An asynchronous <see cref="MetaMystiaNetwork.ConnectToPeer"/> call completed.</summary>
    ConnectionResult,
  }

  // --- NetworkEvent ----------------------------------------------------

  /// <summary>
  /// Carries data for a single network event dequeued during <see cref="NetworkManager.Poll"/>.
  /// Which fields are populated depends on <see cref="Type"/>.
  /// </summary>
  public struct NetworkEvent
  {
    /// <summary>Identifies which event occurred.</summary>
    public NetworkEventType Type;

    /// <summary>
    /// Sender peer ID (<see cref="NetworkEventType.Message"/> /
    /// <see cref="NetworkEventType.PeerStatus"/>), new leader ID
    /// (<see cref="NetworkEventType.LeaderChanged"/>), or target address
    /// (<see cref="NetworkEventType.ConnectionResult"/>).
    /// </summary>
    public string PeerId = string.Empty;

    /// <summary>
    /// User message type (≥ <c>0x0100</c>).
    /// Populated for <see cref="NetworkEventType.Message"/> only.
    /// </summary>
    public ushort MsgType;

    /// <summary>
    /// Message payload bytes.
    /// Non-<c>null</c> for <see cref="NetworkEventType.Message"/> only.
    /// </summary>
    public byte[]? Data;

    /// <summary>
    /// New peer state.
    /// Populated for <see cref="NetworkEventType.PeerStatus"/> only.
    /// </summary>
    public PeerStatus Status;

    /// <summary>
    /// <c>true</c> if the connection attempt succeeded.
    /// Populated for <see cref="NetworkEventType.ConnectionResult"/> only.
    /// </summary>
    public bool Success;

    /// <summary>
    /// Error code when <see cref="Success"/> is <c>false</c>.
    /// See <see cref="NetErrorCode"/>.
    /// </summary>
    public int ErrorCode;

    /// <summary>Initializes all fields to their safe defaults.</summary>
    public NetworkEvent() { }
  }

  // --- NetworkConfigFFI ------------------------------------------------

  /// <summary>
  /// C-compatible runtime configuration passed to the Rust library via
  /// <see cref="MetaMystiaNetwork.InitializeNetworkWithConfig"/>.
  /// Use <see cref="Default"/> to obtain safe defaults, then modify individual
  /// fields before passing by <c>ref</c>.
  /// </summary>
  /// <remarks>
  /// <para>
  /// The field layout is <c>#[repr(C)]</c>-compatible with the Rust struct.
  /// <b>Do not reorder fields or change types</b> without also updating the Rust
  /// side. The struct occupies exactly 80 bytes:
  /// </para>
  /// <code>
  ///  offset  0  heartbeat_interval_ms        u64  (8 B)
  ///  offset  8  election_timeout_min_ms      u64  (8 B)
  ///  offset 16  election_timeout_max_ms      u64  (8 B)
  ///  offset 24  heartbeat_timeout_multiplier u32  (4 B)
  ///  offset 28  [4 B alignment padding]
  ///  offset 32  reconnect_initial_ms         u64  (8 B)
  ///  offset 40  reconnect_max_ms             u64  (8 B)
  ///  offset 48  compression_threshold        u32  (4 B)
  ///  offset 52  send_queue_capacity          u32  (4 B)
  ///  offset 56  max_connections              u32  (4 B)
  ///  offset 60  max_message_size             u32  (4 B)
  ///  offset 64  centralized_auto_forward     u8   (1 B)
  ///  offset 65  auto_election_enabled        u8   (1 B)
  ///  offset 66  mdns_port                    u16  (2 B)
  ///  offset 68  manual_override_recovery     u8   (1 B)
  ///  offset 69  [3 B alignment padding]
  ///  offset 72  handshake_timeout_ms         u64  (8 B)
  ///  sizeof = 80
  /// </code>
  /// </remarks>
  [StructLayout(LayoutKind.Sequential)]
  public struct NetworkConfigFFI
  {
    /// <summary>Interval between heartbeat / Ping broadcasts (ms). Default: 500.</summary>
    public ulong heartbeat_interval_ms;

    /// <summary>
    /// Minimum randomized election timeout (ms).
    /// Must be greater than <see cref="heartbeat_interval_ms"/>. Default: 1500.
    /// </summary>
    public ulong election_timeout_min_ms;

    /// <summary>
    /// Maximum randomized election timeout (ms).
    /// Must be ≥ <see cref="election_timeout_min_ms"/>. Default: 3000.
    /// </summary>
    public ulong election_timeout_max_ms;

    /// <summary>
    /// Number of missed heartbeat cycles before a peer is declared offline.
    /// <b>Must be <c>uint</c> (32-bit) to match the Rust <c>u32</c>.</b>
    /// Default: 3.
    /// </summary>
    public uint heartbeat_timeout_multiplier;

    /// <summary>Initial exponential back-off delay for reconnection attempts (ms). Default: 1000.</summary>
    public ulong reconnect_initial_ms;

    /// <summary>Maximum back-off delay for reconnection attempts (ms). Default: 30 000.</summary>
    public ulong reconnect_max_ms;

    /// <summary>
    /// LZ4 compression threshold (bytes). Payloads larger than this value are
    /// automatically compressed. Set to <c>0</c> to disable. Default: 512.
    /// </summary>
    public uint compression_threshold;

    /// <summary>
    /// Capacity of each peer's outbound send queue (packets). Must be 0.
    /// Exceeding the capacity returns <see cref="NetErrorCode.SendQueueFull"/>.
    /// Default: 128.
    /// </summary>
    public uint send_queue_capacity;

    /// <summary>
    /// Maximum number of simultaneous TCP connections. Must be 0.
    /// Exceeding this limit returns <see cref="NetErrorCode.MaxConnectionsReached"/>.
    /// Default: 64.
    /// </summary>
    public uint max_connections;

    /// <summary>
    /// Maximum allowed message size in bytes. Must be > 0.
    /// Default: 262 144 (256 KiB).
    /// </summary>
    public uint max_message_size;

    /// <summary>
    /// Centralized-mode Leader behavior: <c>1</c> = automatically re-broadcast
    /// forwarded messages to all followers; <c>0</c> = C# must call
    /// <see cref="MetaMystiaNetwork.ForwardMessage"/> explicitly. Default: 1.
    /// </summary>
    public byte centralized_auto_forward;

    /// <summary>
    /// <c>1</c> = enable automatic Raft leader election;
    /// <c>0</c> = only manual <see cref="MetaMystiaNetwork.SetLeader"/> is honoured.
    /// Default: 1.
    /// </summary>
    public byte auto_election_enabled;

    /// <summary>
    /// UDP port for mDNS peer discovery. Both publisher and browser must use
    /// the same port. Standard mDNS uses 5353 but the Windows DNS Client
    /// occupies that port; <c>15353</c> avoids the conflict. Default: 15353.
    /// </summary>
    public ushort mdns_port;

    /// <summary>
    /// Behaviour when a manually-assigned leader goes offline.
    /// Only relevant when <c>manual_override</c> is active.
    /// <c>0</c> = <see cref="ManualOverrideRecovery.Hold"/> (default),
    /// <c>1</c> = <see cref="ManualOverrideRecovery.AutoElect"/>.
    /// </summary>
    public byte manual_override_recovery;

    // 3 bytes of alignment padding (implicit in LayoutKind.Sequential)

    /// <summary>
    /// Timeout (ms) to complete the TCP handshake with a remote peer.
    /// Must be 0. Default: 5000.
    /// </summary>
    public ulong handshake_timeout_ms;

    /// <summary>
    /// Returns a <see cref="NetworkConfigFFI"/> pre-filled with the same defaults
    /// used by <see cref="MetaMystiaNetwork.InitializeNetwork"/>.
    /// </summary>
    public static NetworkConfigFFI Default() => new()
    {
      heartbeat_interval_ms = 500,
      election_timeout_min_ms = 1500,
      election_timeout_max_ms = 3000,
      heartbeat_timeout_multiplier = 3,
      reconnect_initial_ms = 1000,
      reconnect_max_ms = 30_000,
      compression_threshold = 512,
      send_queue_capacity = 128,
      max_connections = 64,
      max_message_size = 256 * 1024,
      centralized_auto_forward = 1,
      auto_election_enabled = 1,
      mdns_port = 15353,
      manual_override_recovery = (byte)ManualOverrideRecovery.Hold,
      handshake_timeout_ms = 5000,
    };
  }

  // --- Callback delegate types ---------------------------------------------

  /// <summary>
  /// Invoked on a Rust background thread when a user message arrives.
  /// </summary>
  /// <remarks>
  /// <para>
  /// <b>All pointer arguments are invalidated when this method returns.</b>
  /// Copy <paramref name="peerId"/> with <see cref="Marshal.PtrToStringAnsi(IntPtr)"/>
  /// and copy <paramref name="data"/> with
  /// <see cref="Marshal.Copy(IntPtr, byte[], int, int)"/> before returning.
  /// </para>
  /// <para>
  /// Do <b>not</b> call any FFI function from inside this callback; doing so may
  /// deadlock. Enqueue the copied data and process it on the main thread via
  /// <see cref="NetworkManager.Poll"/>.
  /// </para>
  /// <para>
  /// On IL2CPP targets decorate the implementing method with
  /// <c>[AOT.MonoPInvokeCallback(typeof(ReceiveCallback))]</c>.
  /// </para>
  /// </remarks>
  [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
  public delegate void ReceiveCallback(IntPtr peerId, IntPtr data, int length, ushort msgType, byte flags);

  /// <summary>
  /// Invoked on a Rust background thread when the Leader peer changes.
  /// </summary>
  /// <remarks>See <see cref="ReceiveCallback"/> for threading and lifetime constraints.</remarks>
  /// <param name="leaderPeerId">
  /// Pointer to the new leader's peer-ID string, or an empty string when there
  /// is currently no leader.
  /// </param>
  [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
  public delegate void LeaderChangedCallback(IntPtr leaderPeerId);

  /// <summary>
  /// Invoked on a Rust background thread when a peer's connection state changes.
  /// </summary>
  /// <remarks>See <see cref="ReceiveCallback"/> for threading and lifetime constraints.</remarks>
  /// <param name="peerId">Pointer to the affected peer's ID string.</param>
  /// <param name="status">New state: 0 = Connected, 1 = Disconnected, 2 = Reconnecting.</param>
  [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
  public delegate void PeerStatusCallback(IntPtr peerId, int status);

  /// <summary>
  /// Invoked on a Rust background thread when an asynchronous
  /// <see cref="MetaMystiaNetwork.ConnectToPeer"/> call completes.
  /// </summary>
  /// <remarks>See <see cref="ReceiveCallback"/> for threading and lifetime constraints.</remarks>
  /// <param name="addr">Pointer to the address string passed to <c>ConnectToPeer</c>.</param>
  /// <param name="success"><c>1</c> if the connection succeeded; <c>0</c> otherwise.</param>
  /// <param name="errorCode"><see cref="NetErrorCode.OK"/> on success or a negative error code.</param>
  [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
  public delegate void ConnectionResultCallback(IntPtr addr, byte success, int errorCode);

  // --- Raw P/Invoke bindings -----------------------------------------------

  /// <summary>
  /// Direct P/Invoke bindings for the native <c>meta_mystia_network</c> library.
  /// Every exported function uses <see cref="CallingConvention.Cdecl"/> to match
  /// the Rust <c>extern "C"</c> ABI.
  /// </summary>
  /// <remarks>
  /// <para>
  /// Functions that return <c>int</c> use <see cref="NetErrorCode"/> values;
  /// zero means success.
  /// </para>
  /// <para>
  /// Functions that return <see cref="IntPtr"/> to a string share a single
  /// internal native buffer: the pointer is valid only until the next call that
  /// also returns a string. Always copy immediately with <see cref="PtrToString"/>.
  /// </para>
  /// <para>
  /// For a higher-level interface with automatic delegate pinning and thread-safe
  /// event delivery, use <see cref="NetworkManager"/>.
  /// </para>
  /// </remarks>
  public static class MetaMystiaNetwork
  {
    private const string DLL = "meta_mystia_network";
    private const CallingConvention CC = CallingConvention.Cdecl;
    private const CharSet CS = CharSet.Ansi;

    #region Lifecycle

    /// <summary>
    /// Initializes the network stack with default configuration, binds a random
    /// TCP port, and starts mDNS discovery.
    /// </summary>
    /// <param name="peerId">
    /// Unique identifier for this node within the session. Must not be empty and
    /// must not duplicate another connected peer's ID.
    /// </param>
    /// <param name="sessionId">
    /// Room / session identifier. Nodes with different session IDs are invisible
    /// to each other even on the same LAN.
    /// </param>
    /// <returns><see cref="NetErrorCode.OK"/> or an error code.</returns>
    [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
    public static extern int InitializeNetwork(string peerId, string sessionId);

    /// <summary>
    /// Initializes the network stack with a caller-supplied configuration.
    /// The configuration is validated before any resources are allocated.
    /// </summary>
    /// <returns>
    /// <see cref="NetErrorCode.OK"/> on success;
    /// <see cref="NetErrorCode.InvalidArgument"/> if the configuration fails validation.
    /// </returns>
    [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
    public static extern int InitializeNetworkWithConfig(string peerId, string sessionId, ref NetworkConfigFFI config);

    /// <summary>
    /// Gracefully shuts down the network: notifies all peers, drains outbound queues,
    /// cancels background tasks, and releases all native resources.
    /// After this call, <see cref="InitializeNetwork"/> may be called again.
    /// </summary>
    /// <returns>
    /// <see cref="NetErrorCode.OK"/>, or <see cref="NetErrorCode.NotInitialized"/>
    /// if the network was not running.
    /// </returns>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int ShutdownNetwork();

    /// <summary>
    /// Returns <c>1</c> if the network is currently initialized, <c>0</c> otherwise.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern byte IsNetworkInitialized();

    /// <summary>
    /// Returns <c>1</c> if mDNS discovery started successfully and is
    /// active, <c>0</c> otherwise (e.g., port conflict, multicast
    /// unavailable, or network not initialized). When <c>0</c>, peer
    /// discovery relies solely on manual <see cref="ConnectToPeer"/> calls.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern byte IsMdnsActive();

    #endregion

    #region Error reporting

    /// <summary>Returns the error code recorded by the most recent failed FFI call.</summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int GetLastErrorCode();

    /// <summary>
    /// Returns a pointer to a human-readable description of the most recent error.
    /// Copy immediately with <see cref="PtrToString"/>; the buffer is reused on the
    /// next string-returning call.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern IntPtr GetLastErrorMessage();

    #endregion

    #region Connection management

    /// <summary>
    /// Asynchronously opens a TCP connection to <paramref name="addr"/>.
    /// Returns <see cref="NetErrorCode.OK"/> immediately once the attempt is queued;
    /// the actual outcome is reported via <see cref="ConnectionResultCallback"/>.
    /// </summary>
    /// <param name="addr">Target address in <c>ip:port</c> format.</param>
    [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
    public static extern int ConnectToPeer(string addr);

    /// <summary>
    /// Sends a <c>PeerLeave</c> notification and closes the connection to
    /// <paramref name="peerId"/>. This does <b>not</b> trigger auto-reconnect.
    /// Silently succeeds if the peer is not connected.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
    public static extern int DisconnectPeer(string peerId);

    #endregion

    #region Local node information

    /// <summary>
    /// Returns a pointer to the local TCP listener address (e.g. <c>"0.0.0.0:54321"</c>).
    /// Remote peers can supply this address to their <see cref="ConnectToPeer"/>.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern IntPtr GetLocalAddr();

    /// <summary>Returns a pointer to the local peer ID supplied at initialization.</summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern IntPtr GetLocalPeerId();

    /// <summary>Returns a pointer to the session ID supplied at initialization.</summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern IntPtr GetSessionId();

    #endregion

    #region Peer queries

    /// <summary>
    /// Returns the number of peers in the <see cref="PeerStatus.Connected"/> state.
    /// Does not count this node itself.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int GetPeerCount();

    /// <summary>
    /// Returns a pointer to a newline-separated list of connected peer IDs,
    /// e.g. <c>"peer_a\npeer_b\n"</c>. An empty string indicates no connected peers.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern IntPtr GetPeerList();

    /// <summary>
    /// Returns the most recently measured round-trip time to <paramref name="peerId"/>
    /// in milliseconds, or <c>-1</c> if the peer is unknown or the RTT is not yet available.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
    public static extern int GetPeerRTT(string peerId);

    /// <summary>
    /// Returns the current <see cref="PeerStatus"/> ordinal for <paramref name="peerId"/>,
    /// or <c>-1</c> if the peer is not known.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
    public static extern int GetPeerStatus(string peerId);

    #endregion

    #region Leader management

    /// <summary>
    /// Manually assigns <paramref name="peerId"/> as the Leader and broadcasts a
    /// <c>LeaderAssign</c> message to all connected peers, overriding any ongoing
    /// automatic election.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
    public static extern int SetLeader(string peerId);

    /// <summary>
    /// Enables (<c>1</c>) or disables (<c>0</c>) automatic Raft leader election.
    /// When disabled, leadership changes only through <see cref="SetLeader"/>.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int EnableAutoLeaderElection(byte enable);

    /// <summary>
    /// Returns a pointer to the current Leader's peer ID, or a pointer to an
    /// empty string if no leader has been elected.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern IntPtr GetCurrentLeader();

    /// <summary>Returns <c>1</c> if this node is currently the Leader; <c>0</c> otherwise.</summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern byte IsLeader();

    #endregion

    #region Centralized mode

    /// <summary>
    /// Enables (<c>1</c>) or disables (<c>0</c>) centralized routing mode.
    /// When enabled, broadcast messages from non-Leader peers are forwarded to the
    /// Leader first, which then rebroadcasts them to all followers.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int SetCentralizedMode(byte enable);

    /// <summary>Returns <c>1</c> if centralized mode is active; <c>0</c> for direct P2P.</summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern byte IsCentralizedMode();

    /// <summary>
    /// Controls whether the Leader automatically re-broadcasts forwarded messages
    /// (<c>1</c>) or defers to C# code via <see cref="ForwardMessage"/> (<c>0</c>).
    /// Only meaningful when centralized mode is active.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int SetCentralizedAutoForward(byte enable);

    /// <summary>Returns the current auto-forward setting: <c>1</c> = enabled.</summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern byte IsCentralizedAutoForward();

    #endregion

    #region Compression

    /// <summary>
    /// Overrides the LZ4 compression threshold at runtime. Payloads larger than
    /// <paramref name="threshold"/> bytes are compressed before transmission.
    /// Pass <c>0</c> to disable compression entirely.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int SetCompressionThreshold(uint threshold);

    #endregion

    #region Messaging

    /// <summary>
    /// Broadcasts <paramref name="data"/> to all currently connected peers.
    /// In centralized mode a non-Leader node sends to the Leader for rebroadcast.
    /// </summary>
    /// <param name="msgType">Application-defined type; must be ≥ <c>0x0100</c>.</param>
    /// <param name="flags">
    /// User-defined flags in bits 1–7. Bit 0 is reserved (LZ4) and is silently
    /// cleared before transmission.
    /// </param>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int BroadcastMessage(byte[] data, int length, ushort msgType, byte flags);

    /// <summary>Sends a unicast message directly to <paramref name="targetPeerId"/>.</summary>
    [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
    public static extern int SendToPeer(string targetPeerId, byte[] data, int length, ushort msgType, byte flags);

    /// <summary>
    /// Sends a message to the current Leader. If this node is the Leader,
    /// the message is handled locally without network I/O.
    /// Returns <see cref="NetErrorCode.NotLeader"/> when no leader is elected.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int SendToLeader(byte[] data, int length, ushort msgType, byte flags);

    /// <summary>
    /// Broadcasts a message originating from the Leader.
    /// Returns <see cref="NetErrorCode.NotLeader"/> if this node is not the Leader.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int SendFromLeader(byte[] data, int length, ushort msgType, byte flags);

    /// <summary>
    /// Leader-only: forwards a message to <paramref name="targetPeerId"/>, or
    /// broadcasts to all peers except <paramref name="fromPeerId"/> when
    /// <paramref name="targetPeerId"/> is <c>null</c>.
    /// Returns <see cref="NetErrorCode.NotLeader"/> if this node is not the Leader.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC, CharSet = CS)]
    public static extern int ForwardMessage(string fromPeerId, string? targetPeerId, byte[] data, int length, ushort msgType, byte flags);

    #endregion

    #region Callback registration

    /// <summary>
    /// Registers the callback invoked when a user message is received.
    /// Pass <c>null</c> to unregister. <b>The delegate must be kept alive in a
    /// <c>static</c> field</b> to prevent GC from collecting the underlying function pointer.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int RegisterReceiveCallback(ReceiveCallback? callback);

    /// <summary>
    /// Registers the callback invoked when the Leader changes.
    /// Pass <c>null</c> to unregister.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int RegisterLeaderChangedCallback(LeaderChangedCallback? callback);

    /// <summary>
    /// Registers the callback invoked when a peer's status changes.
    /// Pass <c>null</c> to unregister.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int RegisterPeerStatusCallback(PeerStatusCallback? callback);

    /// <summary>
    /// Registers the callback invoked when an asynchronous connection attempt completes.
    /// Pass <c>null</c> to unregister.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int RegisterConnectionResultCallback(ConnectionResultCallback? callback);

    #endregion

    #region Logging

    /// <summary>
    /// Enables (<c>1</c>) native Rust logging to stdout via <c>tracing-subscriber</c>.
    /// Requires the library to be compiled with the <c>logging</c> Cargo feature.
    /// Subsequent calls after the first enable are no-ops.
    /// </summary>
    [DllImport(DLL, CallingConvention = CC)]
    public static extern int EnableLogging(byte enable);

    #endregion

    #region Helpers

    /// <summary>
    /// Converts a native C-string pointer returned by the library to a managed
    /// <see cref="string"/>. Returns <see cref="string.Empty"/> for a null pointer.
    /// Must be called before the next FFI function that returns a string pointer,
    /// since the native buffer is reused between calls.
    /// </summary>
    public static string PtrToString(IntPtr ptr) =>
        ptr == IntPtr.Zero ? string.Empty : Marshal.PtrToStringAnsi(ptr) ?? string.Empty;

    /// <summary>
    /// Throws <see cref="InvalidOperationException"/> if <paramref name="code"/>
    /// is not <see cref="NetErrorCode.OK"/>, including the human-readable error
    /// message from <see cref="GetLastErrorMessage"/>.
    /// </summary>
    public static void Check(int code)
    {
      if (code != NetErrorCode.OK)
        throw new InvalidOperationException(
            $"Network error {code}: {PtrToString(GetLastErrorMessage())}");
    }

    #endregion
  }

  // --- High-level NetworkManager — complete usage example ------------------

  /// <summary>
  /// Thread-safe, <see cref="IDisposable"/> wrapper around <see cref="MetaMystiaNetwork"/>
  /// demonstrating the recommended integration pattern.
  /// </summary>
  /// <remarks>
  /// <para><b>Typical usage</b></para>
  /// <code>
  /// // 1. Create manager (process-wide singleton).
  /// using var net = new NetworkManager();
  ///
  /// // 2. Subscribe to events (raised on the thread that calls Poll).
  /// net.MessageReceived   += (peerId, msgType, data) => HandleMessage(peerId, msgType, data);
  /// net.PeerStatusChanged += (peerId, status)        => Log($"{peerId} → {status}");
  /// net.LeaderChanged     += newLeader               => Log($"Leader: {newLeader}");
  /// net.ConnectionResult  += (addr, ok, code)        => Log($"Connect {addr}: {(ok ? "OK" : code)}");
  ///
  /// // 3. Initialize (registers all callbacks internally).
  /// net.Initialize("player_001", "room_42");
  /// Log($"Listening on {net.LocalAddr}");
  ///
  /// // 4. Application / game loop.
  /// while (running)
  /// {
  ///     net.Poll();                                     // dispatches queued events
  ///     net.Broadcast(Encoding.UTF8.GetBytes("hello"), MsgTypeChat);
  ///     Thread.Sleep(16);
  /// }
  /// // Dispose() calls ShutdownNetwork() automatically.
  /// </code>
  /// <para><b>Threading model</b></para>
  /// <para>
  /// Native callbacks (see <see cref="ReceiveCallback"/> etc.) are invoked on a
  /// Rust background thread. They copy pointer-based data into a
  /// <see cref="ConcurrentQueue{T}"/> and return immediately. <see cref="Poll"/>
  /// drains the queue on the caller's thread, raising the C# events there.
  /// </para>
  /// <para>
  /// Do <b>not</b> call any <see cref="MetaMystiaNetwork"/> FFI method from inside
  /// a native callback — doing so will deadlock.
  /// </para>
  /// <para><b>IL2CPP / AOT</b></para>
  /// <para>
  /// On IL2CPP targets the four <c>private static On*Native</c> callback methods
  /// must be decorated with
  /// <c>[AOT.MonoPInvokeCallback(typeof(…))]</c> so the AOT compiler emits the
  /// required reverse-P/Invoke thunks. Compile the project with the <c>IL2CPP</c>
  /// symbol defined to enable those attributes automatically.
  /// </para>
  /// </remarks>
  public sealed class NetworkManager : IDisposable
  {
    // --- User-defined message type example constants (≥ 0x0100) ----------
    //    0x0001–0x00FF are reserved by the internal protocol.
    //    Define your own application message types here or in a separate class.

    // public const ushort MsgTypeChat       = 0x0100;
    // public const ushort MsgTypePlayerMove = 0x0101;
    // public const ushort MsgTypeGameState  = 0x0102;

    // --- Static pinned delegates ------------------------------------------
    //    Must be static so they survive GC for the lifetime of the process.
    //    The native library holds raw function pointers to these methods.
    private static readonly ReceiveCallback s_onReceive = OnReceiveNative;
    private static readonly LeaderChangedCallback s_onLeaderChanged = OnLeaderChangedNative;
    private static readonly PeerStatusCallback s_onPeerStatus = OnPeerStatusNative;
    private static readonly ConnectionResultCallback s_onConnResult = OnConnectionResultNative;

    // --- Cross-thread event queue -----------------------------------------
    //    Native callbacks (Rust thread) enqueue; Poll() (caller thread) dequeues.
    private static readonly ConcurrentQueue<NetworkEvent> s_eventQueue = new();

    // --- Active-instance back-reference -----------------------------------
    //    Static callbacks need this to reach the current instance's C# events.
    private static NetworkManager? s_instance;

    private bool _disposed;

    /// <summary>
    /// Creates a new <see cref="NetworkManager"/>.
    /// </summary>
    /// <exception cref="InvalidOperationException">
    /// A previous instance has not yet been disposed.
    /// The native library is a process-wide singleton; only one manager is
    /// allowed at a time.
    /// </exception>
    public NetworkManager()
    {
      if (s_instance is not null)
        throw new InvalidOperationException(
            "Only one NetworkManager may be active at a time. Dispose the previous instance first.");
      s_instance = this;
    }

    // --- C# events (raised on the thread that calls Poll) -----------------

    /// <summary>
    /// Raised when a user message arrives from a remote peer.
    /// Arguments: <c>(peerId, msgType, payload)</c>.
    /// </summary>
    public event Action<string, ushort, byte[]>? MessageReceived;

    /// <summary>
    /// Raised when a peer's connection state changes.
    /// Arguments: <c>(peerId, newStatus)</c>.
    /// </summary>
    public event Action<string, PeerStatus>? PeerStatusChanged;

    /// <summary>
    /// Raised when the Leader changes.
    /// Argument: new leader's peer ID, or <see cref="string.Empty"/> when there is no leader.
    /// </summary>
    public event Action<string>? LeaderChanged;

    /// <summary>
    /// Raised when an asynchronous <see cref="MetaMystiaNetwork.ConnectToPeer"/> call completes.
    /// Arguments: <c>(targetAddr, succeeded, errorCode)</c>.
    /// </summary>
    public event Action<string, bool, int>? ConnectionResult;

    // --- Properties ------------------------------------------------------

    /// <summary>Local TCP listener address, e.g. <c>"0.0.0.0:54321"</c>.</summary>
    public string LocalAddr => MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetLocalAddr());

    /// <summary>Local peer ID supplied at initialization.</summary>
    public string LocalPeerId => MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetLocalPeerId());

    /// <summary>Session ID supplied at initialization.</summary>
    public string SessionId => MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetSessionId());

    /// <summary><c>true</c> if this node is currently the Leader.</summary>
    public bool IsLeader => MetaMystiaNetwork.IsLeader() != 0;

    /// <summary>Peer ID of the current Leader, or <see cref="string.Empty"/> if none.</summary>
    public string CurrentLeader => MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetCurrentLeader());

    /// <summary>Number of peers currently in the Connected state.</summary>
    public int PeerCount => MetaMystiaNetwork.GetPeerCount();

    /// <summary>
    /// <c>true</c> if mDNS discovery is active. When <c>false</c>, peer
    /// discovery relies on manual <see cref="MetaMystiaNetwork.ConnectToPeer"/> calls.
    /// </summary>
    public bool IsMdnsActive => MetaMystiaNetwork.IsMdnsActive() != 0;

    // --- Lifecycle -------------------------------------------------------

    /// <summary>
    /// Initializes the native network stack with default configuration and
    /// registers all four callbacks.
    /// </summary>
    /// <exception cref="InvalidOperationException">Propagated from <see cref="MetaMystiaNetwork.Check"/> on failure.</exception>
    public void Initialize(string peerId, string sessionId)
    {
      MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetwork(peerId, sessionId));
      RegisterCallbacks();
    }

    /// <summary>
    /// Initializes the native network stack with custom configuration and
    /// registers all four callbacks.
    /// </summary>
    public void Initialize(string peerId, string sessionId, ref NetworkConfigFFI config)
    {
      MetaMystiaNetwork.Check(MetaMystiaNetwork.InitializeNetworkWithConfig(peerId, sessionId, ref config));
      RegisterCallbacks();
    }

    /// <summary>
    /// Gracefully shuts down the network, draining queues and releasing all native
    /// resources. After this call <see cref="Initialize"/> may be called again.
    /// </summary>
    public void Shutdown() => MetaMystiaNetwork.ShutdownNetwork();

    // --- Main-loop pump ---------------------------------------------------

    /// <summary>
    /// Drains all pending network events from the internal queue and raises
    /// the corresponding C# events on the calling thread.
    /// Call once per frame / tick from your application's main loop.
    /// </summary>
    public void Poll()
    {
      while (s_eventQueue.TryDequeue(out NetworkEvent ev))
      {
        switch (ev.Type)
        {
          case NetworkEventType.Message:
            MessageReceived?.Invoke(ev.PeerId, ev.MsgType, ev.Data!);
            break;
          case NetworkEventType.PeerStatus:
            PeerStatusChanged?.Invoke(ev.PeerId, ev.Status);
            break;
          case NetworkEventType.LeaderChanged:
            LeaderChanged?.Invoke(ev.PeerId);
            break;
          case NetworkEventType.ConnectionResult:
            ConnectionResult?.Invoke(ev.PeerId, ev.Success, ev.ErrorCode);
            break;
        }
      }
    }

    // --- Convenience send wrappers ----------------------------------------

    /// <summary>Broadcasts <paramref name="data"/> to all connected peers.</summary>
    /// <param name="msgType">Must be ≥ <c>0x0100</c>.</param>
    public int Broadcast(byte[] data, ushort msgType, byte flags = 0) =>
        MetaMystiaNetwork.BroadcastMessage(data, data.Length, msgType, flags);

    /// <summary>Sends a unicast message to <paramref name="targetPeerId"/>.</summary>
    public int SendTo(string targetPeerId, byte[] data, ushort msgType, byte flags = 0) =>
        MetaMystiaNetwork.SendToPeer(targetPeerId, data, data.Length, msgType, flags);

    /// <summary>Sends a message to the current Leader.</summary>
    public int SendToLeader(byte[] data, ushort msgType, byte flags = 0) =>
        MetaMystiaNetwork.SendToLeader(data, data.Length, msgType, flags);

    /// <summary>
    /// Returns a snapshot of the currently connected peer IDs.
    /// Empty array when no peers are connected.
    /// </summary>
    public string[] GetPeerIds() =>
        MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetPeerList())
                         .Split('\n', StringSplitOptions.RemoveEmptyEntries);

    // --- IDisposable ------------------------------------------------------

    /// <inheritdoc/>
    public void Dispose()
    {
      if (_disposed) return;
      _disposed = true;
      MetaMystiaNetwork.ShutdownNetwork();
      s_instance = null;
    }

    // --- Private helpers --------------------------------------------------

    private static void RegisterCallbacks()
    {
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterReceiveCallback(s_onReceive));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterLeaderChangedCallback(s_onLeaderChanged));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterPeerStatusCallback(s_onPeerStatus));
      MetaMystiaNetwork.Check(MetaMystiaNetwork.RegisterConnectionResultCallback(s_onConnResult));
    }

    // --- Native callbacks ------------------------------------------------
    //    Invoked on a Rust background thread.
    //    Rules: copy pointer data synchronously; do NOT call any FFI function.

#if IL2CPP || ENABLE_IL2CPP
        [AOT.MonoPInvokeCallback(typeof(ReceiveCallback))]
#endif
    private static void OnReceiveNative(IntPtr peerId, IntPtr data, int length, ushort msgType, byte flags)
    {
      byte[] payload = new byte[length];
      Marshal.Copy(data, payload, 0, length);
      s_eventQueue.Enqueue(new NetworkEvent
      {
        Type = NetworkEventType.Message,
        PeerId = Marshal.PtrToStringAnsi(peerId) ?? string.Empty,
        MsgType = msgType,
        Data = payload,
      });
    }

#if IL2CPP || ENABLE_IL2CPP
        [AOT.MonoPInvokeCallback(typeof(LeaderChangedCallback))]
#endif
    private static void OnLeaderChangedNative(IntPtr leaderPeerId)
    {
      s_eventQueue.Enqueue(new NetworkEvent
      {
        Type = NetworkEventType.LeaderChanged,
        PeerId = Marshal.PtrToStringAnsi(leaderPeerId) ?? string.Empty,
      });
    }

#if IL2CPP || ENABLE_IL2CPP
        [AOT.MonoPInvokeCallback(typeof(PeerStatusCallback))]
#endif
    private static void OnPeerStatusNative(IntPtr peerId, int status)
    {
      s_eventQueue.Enqueue(new NetworkEvent
      {
        Type = NetworkEventType.PeerStatus,
        PeerId = Marshal.PtrToStringAnsi(peerId) ?? string.Empty,
        Status = (PeerStatus)status,
      });
    }

#if IL2CPP || ENABLE_IL2CPP
        [AOT.MonoPInvokeCallback(typeof(ConnectionResultCallback))]
#endif
    private static void OnConnectionResultNative(IntPtr addr, byte success, int errorCode)
    {
      s_eventQueue.Enqueue(new NetworkEvent
      {
        Type = NetworkEventType.ConnectionResult,
        PeerId = Marshal.PtrToStringAnsi(addr) ?? string.Empty,
        Success = success != 0,
        ErrorCode = errorCode,
      });
    }
  }
}
