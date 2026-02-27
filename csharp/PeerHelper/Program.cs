using System;
using System.Collections.Concurrent;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using MetaMystiaNetworkBindings;

/// <summary>
/// Standalone helper process used by E2E integration tests.
///
/// Protocol (over stdout / stdin):
///   LISTEN:{addr}            – printed once after initialization
///   PEER:{peerId}:{status}   – printed when a peer connection status changes
///   RECV:{peerId}:{msgType}:{base64payload} – printed when a user message arrives
///   PEERCOUNT:{n}            – printed in response to GETPEERCOUNT
///
///   Stdin commands:
///   STOP                     – graceful shutdown
///   CONNECT:{addr}           – connect to another peer
///   SEND:{msgType}:{base64}  – broadcast a message
///   SENDTO:{peerId}:{msgType}:{base64} – unicast a message
///   GETPEERCOUNT             – query connected peer count
///
/// When a peer connects (status == Connected) the helper automatically
/// broadcasts a "HELLO_FROM_HELPER" greeting message so the test process
/// can verify bidirectional message delivery.
/// </summary>
class PeerHelperProgram
{
  static readonly ConcurrentQueue<NetworkEvent> Events = new();
  static readonly ConcurrentQueue<string> Commands = new();
  static ReceiveCallback? _onRecv;
  static PeerStatusCallback? _onPeerStatus;
  static ConnectionResultCallback? _onConn;
  static volatile bool _shouldStop;

  static int Main(string[] args)
  {
    string peerId = args.Length > 0 ? args[0] : "helper_peer";
    string sessionId = args.Length > 1 ? args[1] : "e2e_session";

    int rc = MetaMystiaNetwork.InitializeNetwork(peerId, sessionId);
    if (rc != NetErrorCode.OK)
    {
      Console.Error.WriteLine($"InitializeNetwork failed with code {rc}");
      return 1;
    }

    _onRecv = OnReceive;
    _onPeerStatus = OnPeerStatus;
    _onConn = OnConnectionResult;
    MetaMystiaNetwork.RegisterReceiveCallback(_onRecv);
    MetaMystiaNetwork.RegisterPeerStatusCallback(_onPeerStatus);
    MetaMystiaNetwork.RegisterConnectionResultCallback(_onConn);

    string addr = MetaMystiaNetwork.PtrToString(MetaMystiaNetwork.GetLocalAddr());
    Console.WriteLine($"LISTEN:{addr}");
    Console.Out.Flush();

    // Read stdin in a background thread so the main loop is free to
    // process events and send messages.
    var stdinThread = new Thread(() =>
    {
      try
      {
        while (!_shouldStop)
        {
          string? line = Console.ReadLine();
          if (line == null || line.Trim() == "STOP")
          {
            _shouldStop = true;
            break;
          }
          Commands.Enqueue(line.Trim());
        }
      }
      catch { _shouldStop = true; }
    })
    { IsBackground = true };
    stdinThread.Start();

    // Main event loop – drains the concurrent queue and acts on events.
    while (!_shouldStop)
    {
      // Process commands from stdin
      while (Commands.TryDequeue(out var cmd))
      {
        ProcessCommand(cmd);
      }

      while (Events.TryDequeue(out var ev))
      {
        switch (ev.Type)
        {
          case NetworkEventType.PeerStatus:
            Console.WriteLine($"PEER:{ev.PeerId}:{(int)ev.Status}");
            Console.Out.Flush();
            if (ev.Status == PeerStatus.Connected)
            {
              // Auto-send a greeting so the test can verify receipt.
              byte[] greeting = Encoding.UTF8.GetBytes("HELLO_FROM_HELPER");
              MetaMystiaNetwork.BroadcastMessage(greeting, greeting.Length, 0x0100, 0);
            }
            break;

          case NetworkEventType.Message:
            string b64 = Convert.ToBase64String(ev.Data ?? Array.Empty<byte>());
            Console.WriteLine($"RECV:{ev.PeerId}:{ev.MsgType}:{b64}");
            Console.Out.Flush();
            break;
        }
      }

      Thread.Sleep(50);
    }

    MetaMystiaNetwork.ShutdownNetwork();
    return 0;
  }

  // --- Native callbacks (Rust thread) ----------------------------------
  // Copy pointer data immediately; do NOT call any FFI function here.

  static void OnReceive(IntPtr peerId, IntPtr data, int length, ushort msgType, byte flags)
  {
    byte[] buf = new byte[length];
    Marshal.Copy(data, buf, 0, length);
    Events.Enqueue(new NetworkEvent
    {
      Type = NetworkEventType.Message,
      PeerId = MetaMystiaNetwork.PtrToString(peerId),
      MsgType = msgType,
      Data = buf,
    });
  }

  static void OnPeerStatus(IntPtr peerId, int status)
  {
    Events.Enqueue(new NetworkEvent
    {
      Type = NetworkEventType.PeerStatus,
      PeerId = MetaMystiaNetwork.PtrToString(peerId),
      Status = (PeerStatus)status,
    });
  }

  static void OnConnectionResult(IntPtr addr, byte success, int errorCode)
  {
    string a = MetaMystiaNetwork.PtrToString(addr);
    Console.WriteLine($"CONNRESULT:{a}:{success}:{errorCode}");
    Console.Out.Flush();
  }

  static void ProcessCommand(string cmd)
  {
    if (cmd.StartsWith("CONNECT:"))
    {
      string target = cmd.Substring("CONNECT:".Length);
      MetaMystiaNetwork.ConnectToPeer(target);
    }
    else if (cmd.StartsWith("SEND:"))
    {
      // SEND:{msgType}:{base64payload}
      var parts = cmd.Substring("SEND:".Length).Split(':', 2);
      if (parts.Length == 2 && ushort.TryParse(parts[0], out ushort mt))
      {
        byte[] data = Convert.FromBase64String(parts[1]);
        MetaMystiaNetwork.BroadcastMessage(data, data.Length, mt, 0);
      }
    }
    else if (cmd.StartsWith("SENDTO:"))
    {
      // SENDTO:{peerId}:{msgType}:{base64payload}
      var parts = cmd.Substring("SENDTO:".Length).Split(':', 3);
      if (parts.Length == 3 && ushort.TryParse(parts[1], out ushort mt))
      {
        byte[] data = Convert.FromBase64String(parts[2]);
        MetaMystiaNetwork.SendToPeer(parts[0], data, data.Length, mt, 0);
      }
    }
    else if (cmd == "GETPEERCOUNT")
    {
      int count = MetaMystiaNetwork.GetPeerCount();
      Console.WriteLine($"PEERCOUNT:{count}");
      Console.Out.Flush();
    }
  }
}
