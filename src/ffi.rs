//! FFI boundary exposing a C-compatible API for the network library.
//!
//! Each exported function is wrapped in `catch_unwind` to prevent
//! Rust panics from crossing the language boundary. Errors are
//! reported via `GetLastErrorCode`/`GetLastErrorMessage`.

use std::ffi::{CStr, CString, c_char};
use std::panic::catch_unwind;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::NetworkState;
use crate::callback::{
    ConnectionResultCallback, LeaderChangedCallback, PeerStatusCallback, ReceiveCallback,
};
use crate::config::NetworkConfig;
use crate::error::NetworkError;
use crate::error::error_codes;
use crate::messaging::encode_internal;
use crate::protocol::InternalMessage;
use crate::types::{ForwardTarget, MessageTarget, PeerId};

// --- globals ---------------------------------------------------------
// The Tokio runtime used to drive async components. Created during
// initialization and torn down during shutdown.
static RUNTIME: Mutex<Option<tokio::runtime::Runtime>> = Mutex::new(None);

// Singleton network state exposed to FFI callers.
static NETWORK: Mutex<Option<NetworkState>> = Mutex::new(None);

// Last error code/message pair recorded by any FFI entrypoint. The
// C consumer can retrieve these after a failure.
static LAST_ERROR: Mutex<Option<(i32, String)>> = Mutex::new(None);

// Storage for a string returned to C; kept alive until the next call.
static LAST_RETURNED_STRING: Mutex<Option<CString>> = Mutex::new(None);

/// Record an error code and human-readable message for later
/// retrieval from the FFI layer.
fn set_error(code: i32, msg: impl Into<String>) {
    *LAST_ERROR.lock() = Some((code, msg.into()));
}

fn set_network_error(e: &NetworkError) {
    set_error(e.error_code(), e.to_string());
}

/// Convert a Rust `String` into a C pointer that remains valid
/// until the next call that returns a string. Handles embedded NUL
/// bytes by logging and returning an empty string.
fn return_string(s: String) -> *const c_char {
    let c_string = match CString::new(s) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "string contains NUL byte, returning empty");
            CString::default()
        }
    };
    let ptr = c_string.as_ptr();

    *LAST_RETURNED_STRING.lock() = Some(c_string);

    ptr
}

unsafe fn c_str_to_string(ptr: *const c_char) -> Result<String, NetworkError> {
    if ptr.is_null() {
        return Err(NetworkError::InvalidArgument("null string pointer".into()));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(|s| s.to_owned())
        .map_err(|e| NetworkError::InvalidArgument(format!("invalid UTF-8: {e}")))
}

fn with_network<F, R>(f: F) -> Result<R, NetworkError>
where
    F: FnOnce(&NetworkState) -> Result<R, NetworkError>,
{
    let guard = NETWORK.lock();
    let state = guard.as_ref().ok_or(NetworkError::NotInitialized)?;
    f(state)
}

/// C-compatible layout for network configuration. Field order mirrors
/// the Rust `NetworkConfig` and groups timing, sizing and feature
/// toggle parameters together.
///
/// # Layout (80 bytes, 8-byte aligned)
///
/// ```text
/// offset  0  heartbeat_interval_ms        u64  (8 B)
/// offset  8  election_timeout_min_ms      u64  (8 B)
/// offset 16  election_timeout_max_ms      u64  (8 B)
/// offset 24  heartbeat_timeout_multiplier u32  (4 B)
/// offset 28  [4 B implicit padding]
/// offset 32  reconnect_initial_ms         u64  (8 B)
/// offset 40  reconnect_max_ms             u64  (8 B)
/// offset 48  compression_threshold        u32  (4 B)
/// offset 52  send_queue_capacity          u32  (4 B)
/// offset 56  max_connections              u32  (4 B)
/// offset 60  max_message_size             u32  (4 B)
/// offset 64  centralized_auto_forward     u8   (1 B)
/// offset 65  auto_election_enabled        u8   (1 B)
/// offset 66  mdns_port                    u16  (2 B)
/// offset 68  manual_override_recovery     u8   (1 B)
/// offset 69  [3 B explicit padding]
/// offset 72  handshake_timeout_ms         u64  (8 B)
/// sizeof = 80
/// ```
#[repr(C)]
pub struct NetworkConfigFFI {
    // heartbeat/election timing
    pub heartbeat_interval_ms: u64,
    pub election_timeout_min_ms: u64,
    pub election_timeout_max_ms: u64,
    pub heartbeat_timeout_multiplier: u32,

    // reconnection/backoff timing
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,

    // payload and queue sizes
    pub compression_threshold: u32,
    pub send_queue_capacity: u32,

    // connection limits
    pub max_connections: u32,

    // maximum message size
    pub max_message_size: u32,

    // feature toggles (use 0/1)
    pub centralized_auto_forward: u8,
    pub auto_election_enabled: u8,

    // mDNS discovery port
    pub mdns_port: u16,

    // manual override recovery strategy (0 = Hold, 1 = AutoElect)
    pub manual_override_recovery: u8,

    // explicit padding to maintain alignment
    pub(crate) _padding: [u8; 3],

    // handshake timeout
    pub handshake_timeout_ms: u64,
}

impl From<&NetworkConfigFFI> for NetworkConfig {
    fn from(ffi: &NetworkConfigFFI) -> Self {
        Self {
            heartbeat_interval_ms: ffi.heartbeat_interval_ms,
            election_timeout_min_ms: ffi.election_timeout_min_ms,
            election_timeout_max_ms: ffi.election_timeout_max_ms,
            heartbeat_timeout_multiplier: ffi.heartbeat_timeout_multiplier,
            reconnect_initial_ms: ffi.reconnect_initial_ms,
            reconnect_max_ms: ffi.reconnect_max_ms,
            compression_threshold: ffi.compression_threshold,
            send_queue_capacity: ffi.send_queue_capacity as usize,
            max_connections: ffi.max_connections as usize,
            max_message_size: ffi.max_message_size,
            handshake_timeout_ms: ffi.handshake_timeout_ms,
            mdns_port: ffi.mdns_port,
            centralized_auto_forward: ffi.centralized_auto_forward != 0,
            auto_election_enabled: ffi.auto_election_enabled != 0,
            manual_override_recovery: crate::config::ManualOverrideRecovery::from_u8(
                ffi.manual_override_recovery,
            ),
        }
    }
}

/// Initialize the network stack with the given peer and session IDs.
/// Returns 0 (`error_codes::OK`) on success or an error code on failure.
#[unsafe(no_mangle)]
pub extern "C" fn InitializeNetwork(peer_id: *const c_char, session_id: *const c_char) -> i32 {
    catch_unwind(|| initialize_network_inner(peer_id, session_id, None)).unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in InitializeNetwork");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn InitializeNetworkWithConfig(
    peer_id: *const c_char,
    session_id: *const c_char,
    config: *const NetworkConfigFFI,
) -> i32 {
    catch_unwind(|| {
        if config.is_null() {
            set_error(error_codes::INVALID_ARGUMENT, "null config pointer");
            return error_codes::INVALID_ARGUMENT;
        }
        let ffi_config = unsafe { &*config };
        let config = NetworkConfig::from(ffi_config);
        initialize_network_inner(peer_id, session_id, Some(config))
    })
    .unwrap_or_else(|_| {
        set_error(
            error_codes::INTERNAL_ERROR,
            "panic in InitializeNetworkWithConfig",
        );
        error_codes::INTERNAL_ERROR
    })
}

fn initialize_network_inner(
    peer_id_ptr: *const c_char,
    session_id_ptr: *const c_char,
    config: Option<NetworkConfig>,
) -> i32 {
    if NETWORK.lock().is_some() {
        set_error(error_codes::ALREADY_INITIALIZED, "already initialized");
        return error_codes::ALREADY_INITIALIZED;
    }

    let peer_id = match unsafe { c_str_to_string(peer_id_ptr) } {
        Ok(s) => s,
        Err(e) => {
            set_network_error(&e);
            return e.error_code();
        }
    };

    let session_id = match unsafe { c_str_to_string(session_id_ptr) } {
        Ok(s) => s,
        Err(e) => {
            set_network_error(&e);
            return e.error_code();
        }
    };

    let config = config.unwrap_or_default();
    if let Err(e) = config.validate() {
        set_network_error(&e);
        return e.error_code();
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            let err = NetworkError::Internal(format!("failed to create runtime: {e}"));
            set_network_error(&err);
            return err.error_code();
        }
    };

    let state = match rt.block_on(NetworkState::new(peer_id, session_id, config)) {
        Ok(s) => s,
        Err(e) => {
            set_network_error(&e);
            return e.error_code();
        }
    };

    *NETWORK.lock() = Some(state);
    *RUNTIME.lock() = Some(rt);

    error_codes::OK
}

/// Cleanly shut down the network, cancelling the runtime and
/// releasing resources. Safe to call multiple times.
#[unsafe(no_mangle)]
pub extern "C" fn ShutdownNetwork() -> i32 {
    catch_unwind(|| {
        let state = NETWORK.lock().take();
        let Some(state) = state else {
            set_error(error_codes::NOT_INITIALIZED, "not initialized");
            return error_codes::NOT_INITIALIZED;
        };

        {
            let rt_guard = RUNTIME.lock();
            if let Some(rt) = rt_guard.as_ref() {
                rt.block_on(state.shutdown());
            }
        }

        if let Some(rt) = RUNTIME.lock().take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }

        error_codes::OK
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in ShutdownNetwork");
        error_codes::INTERNAL_ERROR
    })
}

/// Query whether `InitializeNetwork` has been called successfully.
/// Returns 1 if initialized, 0 otherwise.
// --- state query --------------------------------------------------------
#[unsafe(no_mangle)]
pub extern "C" fn IsNetworkInitialized() -> u8 {
    catch_unwind(|| u8::from(NETWORK.lock().is_some())).unwrap_or(0)
}

/// Query whether mDNS discovery is currently active.
/// Returns 1 if mDNS started successfully, 0 otherwise
/// (e.g., port conflict, multicast not available, or not initialized).
#[unsafe(no_mangle)]
pub extern "C" fn IsMdnsActive() -> u8 {
    catch_unwind(|| with_network(|state| Ok(u8::from(state.is_mdns_active()))).unwrap_or(0))
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn GetLastErrorCode() -> i32 {
    catch_unwind(|| {
        LAST_ERROR
            .lock()
            .as_ref()
            .map(|(code, _)| *code)
            .unwrap_or(error_codes::OK)
    })
    .unwrap_or(error_codes::INTERNAL_ERROR)
}

#[unsafe(no_mangle)]
pub extern "C" fn GetLastErrorMessage() -> *const c_char {
    catch_unwind(|| {
        let msg = LAST_ERROR
            .lock()
            .as_ref()
            .map(|(_, msg)| msg.clone())
            .unwrap_or_default();
        return_string(msg)
    })
    .unwrap_or(std::ptr::null())
}

// --- peer connection APIs -----------------------------------------------
#[unsafe(no_mangle)]
pub extern "C" fn ConnectToPeer(addr: *const c_char) -> i32 {
    catch_unwind(|| {
        let addr_str = match unsafe { c_str_to_string(addr) } {
            Ok(s) => s,
            Err(e) => {
                set_network_error(&e);
                return e.error_code();
            }
        };

        let (transport, callback) = {
            let guard = NETWORK.lock();
            let state = match guard.as_ref() {
                Some(s) => s,
                None => {
                    set_network_error(&NetworkError::NotInitialized);
                    return error_codes::NOT_INITIALIZED;
                }
            };
            (Arc::clone(&state.transport), Arc::clone(&state.callback))
        };

        let addr_clone = addr_str;
        let rt_guard = RUNTIME.lock();
        if let Some(rt) = rt_guard.as_ref() {
            rt.spawn(async move {
                match transport.connect_to(&addr_clone).await {
                    Ok(_) => {
                        callback.send_connection_result(&addr_clone, true, error_codes::OK);
                    }
                    Err(NetworkError::AlreadyConnected(_)) => {
                        callback.send_connection_result(
                            &addr_clone,
                            true,
                            error_codes::ALREADY_CONNECTED,
                        );
                    }
                    Err(e) => {
                        callback.send_connection_result(&addr_clone, false, e.error_code());
                    }
                }
            });
        }

        let result: Result<(), NetworkError> = Ok(());

        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in ConnectToPeer");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn DisconnectPeer(peer_id: *const c_char) -> i32 {
    catch_unwind(|| {
        let pid_str = match unsafe { c_str_to_string(peer_id) } {
            Ok(s) => s,
            Err(e) => {
                set_network_error(&e);
                return e.error_code();
            }
        };
        let pid = PeerId::new(&pid_str);

        let result = with_network(|state| {
            let transport = Arc::clone(&state.transport);
            let membership = Arc::clone(&state.membership);

            transport.disconnect_peer(&pid)?;
            membership.remove_peer(&pid);
            Ok(())
        });

        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in DisconnectPeer");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn GetLocalAddr() -> *const c_char {
    catch_unwind(
        || match with_network(|state| Ok(state.transport.listener_addr().to_string())) {
            Ok(addr) => return_string(addr),
            Err(_) => std::ptr::null(),
        },
    )
    .unwrap_or(std::ptr::null())
}

#[unsafe(no_mangle)]
pub extern "C" fn GetLocalPeerId() -> *const c_char {
    catch_unwind(
        || match with_network(|state| Ok(state.local_peer_id.as_str().to_owned())) {
            Ok(id) => return_string(id),
            Err(_) => std::ptr::null(),
        },
    )
    .unwrap_or(std::ptr::null())
}

#[unsafe(no_mangle)]
pub extern "C" fn GetSessionId() -> *const c_char {
    catch_unwind(
        || match with_network(|state| Ok(state.session_id.clone())) {
            Ok(id) => return_string(id),
            Err(_) => std::ptr::null(),
        },
    )
    .unwrap_or(std::ptr::null())
}

#[unsafe(no_mangle)]
pub extern "C" fn GetPeerCount() -> i32 {
    catch_unwind(|| {
        with_network(|state| Ok(state.membership.get_connected_peer_count() as i32)).unwrap_or(-1)
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in GetPeerCount");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn GetPeerList() -> *const c_char {
    catch_unwind(|| {
        match with_network(|state| {
            let peers = state.membership.get_connected_peers();
            let list: Vec<String> = peers.iter().map(|p| p.as_str().to_owned()).collect();
            Ok(list.join("\n"))
        }) {
            Ok(list) => return_string(list),
            Err(_) => std::ptr::null(),
        }
    })
    .unwrap_or(std::ptr::null())
}

#[unsafe(no_mangle)]
pub extern "C" fn GetPeerRTT(peer_id: *const c_char) -> i32 {
    catch_unwind(|| {
        let pid_str = match unsafe { c_str_to_string(peer_id) } {
            Ok(s) => s,
            Err(_) => return -1,
        };
        let pid = PeerId::new(&pid_str);
        with_network(|state| {
            Ok(state
                .membership
                .get_peer_rtt(&pid)
                .map(|r| r as i32)
                .unwrap_or(-1))
        })
        .unwrap_or(-1)
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in GetPeerRTT");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn GetPeerStatus(peer_id: *const c_char) -> i32 {
    catch_unwind(|| {
        let pid_str = match unsafe { c_str_to_string(peer_id) } {
            Ok(s) => s,
            Err(_) => return -1,
        };
        let pid = PeerId::new(&pid_str);
        with_network(|state| {
            Ok(state
                .membership
                .get_peer_status(&pid)
                .map(|s| s.as_i32())
                .unwrap_or(-1))
        })
        .unwrap_or(-1)
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in GetPeerStatus");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn SetLeader(peer_id: *const c_char) -> i32 {
    catch_unwind(|| {
        let pid_str = match unsafe { c_str_to_string(peer_id) } {
            Ok(s) => s,
            Err(e) => {
                set_network_error(&e);
                return e.error_code();
            }
        };
        let pid = PeerId::new(&pid_str);

        let result = with_network(|state| {
            let (term, leader_id, assigner_id) = state.leader_election.set_leader(&pid);
            let assign = encode_internal(
                &InternalMessage::LeaderAssign {
                    term,
                    leader_id,
                    assigner_id,
                },
                state.config.max_message_size,
            )?;
            state.transport.broadcast(assign, None);
            Ok(())
        });

        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in SetLeader");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn EnableAutoLeaderElection(enable: u8) -> i32 {
    catch_unwind(|| {
        let result = with_network(|state| {
            state.leader_election.enable_auto_election(enable != 0);
            Ok(())
        });
        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(
            error_codes::INTERNAL_ERROR,
            "panic in EnableAutoLeaderElection",
        );
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn GetCurrentLeader() -> *const c_char {
    catch_unwind(|| {
        match with_network(|state| {
            Ok(state
                .leader_election
                .leader_id()
                .map(|p| p.as_str().to_owned())
                .unwrap_or_default())
        }) {
            Ok(leader) => return_string(leader),
            Err(_) => std::ptr::null(),
        }
    })
    .unwrap_or(std::ptr::null())
}

#[unsafe(no_mangle)]
pub extern "C" fn IsLeader() -> u8 {
    catch_unwind(|| {
        with_network(|state| Ok(u8::from(state.leader_election.is_leader()))).unwrap_or(0)
    })
    .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn SetCentralizedMode(enable: u8) -> i32 {
    catch_unwind(|| {
        let result = with_network(|state| {
            state.session_router.set_centralized(enable != 0);
            Ok(())
        });
        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in SetCentralizedMode");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn IsCentralizedMode() -> u8 {
    catch_unwind(|| {
        with_network(|state| Ok(u8::from(state.session_router.is_centralized()))).unwrap_or(0)
    })
    .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn SetCentralizedAutoForward(enable: u8) -> i32 {
    catch_unwind(|| {
        let result = with_network(|state| {
            state.session_router.set_auto_forward(enable != 0);
            Ok(())
        });
        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(
            error_codes::INTERNAL_ERROR,
            "panic in SetCentralizedAutoForward",
        );
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn IsCentralizedAutoForward() -> u8 {
    catch_unwind(|| {
        with_network(|state| Ok(u8::from(state.session_router.is_auto_forward()))).unwrap_or(0)
    })
    .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn SetCompressionThreshold(threshold: u32) -> i32 {
    catch_unwind(|| {
        let result = with_network(|state| {
            state.session_router.set_compression_threshold(threshold);
            Ok(())
        });
        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(
            error_codes::INTERNAL_ERROR,
            "panic in SetCompressionThreshold",
        );
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn BroadcastMessage(data: *const u8, length: i32, msg_type: u16, flags: u8) -> i32 {
    catch_unwind(|| {
        if data.is_null() || length < 0 {
            set_error(
                error_codes::INVALID_ARGUMENT,
                "invalid data pointer or length",
            );
            return error_codes::INVALID_ARGUMENT;
        }
        let slice = unsafe { std::slice::from_raw_parts(data, length as usize) };

        let result = with_network(|state| {
            if length as u32 > state.config.max_message_size {
                return Err(NetworkError::InvalidArgument(
                    "message exceeds max_message_size".into(),
                ));
            }
            state
                .session_router
                .route_message(MessageTarget::Broadcast, msg_type, slice, flags)
        });

        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in BroadcastMessage");
        error_codes::INTERNAL_ERROR
    })
}

// --- messaging APIs -----------------------------------------------------
#[unsafe(no_mangle)]
pub extern "C" fn SendToPeer(
    target: *const c_char,
    data: *const u8,
    length: i32,
    msg_type: u16,
    flags: u8,
) -> i32 {
    catch_unwind(|| {
        let target_str = match unsafe { c_str_to_string(target) } {
            Ok(s) => s,
            Err(e) => {
                set_network_error(&e);
                return e.error_code();
            }
        };
        if data.is_null() || length < 0 {
            set_error(
                error_codes::INVALID_ARGUMENT,
                "invalid data pointer or length",
            );
            return error_codes::INVALID_ARGUMENT;
        }
        let slice = unsafe { std::slice::from_raw_parts(data, length as usize) };
        let pid = PeerId::new(&target_str);

        let result = with_network(|state| {
            if length as u32 > state.config.max_message_size {
                return Err(NetworkError::InvalidArgument(
                    "message exceeds max_message_size".into(),
                ));
            }
            state.session_router.route_message(
                MessageTarget::ToPeer(pid.clone()),
                msg_type,
                slice,
                flags,
            )
        });

        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in SendToPeer");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn SendToLeader(data: *const u8, length: i32, msg_type: u16, flags: u8) -> i32 {
    catch_unwind(|| {
        if data.is_null() || length < 0 {
            set_error(
                error_codes::INVALID_ARGUMENT,
                "invalid data pointer or length",
            );
            return error_codes::INVALID_ARGUMENT;
        }
        let slice = unsafe { std::slice::from_raw_parts(data, length as usize) };

        let result = with_network(|state| {
            if length as u32 > state.config.max_message_size {
                return Err(NetworkError::InvalidArgument(
                    "message exceeds max_message_size".into(),
                ));
            }
            state
                .session_router
                .route_message(MessageTarget::ToLeader, msg_type, slice, flags)
        });

        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in SendToLeader");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn SendFromLeader(data: *const u8, length: i32, msg_type: u16, flags: u8) -> i32 {
    catch_unwind(|| {
        if data.is_null() || length < 0 {
            set_error(
                error_codes::INVALID_ARGUMENT,
                "invalid data pointer or length",
            );
            return error_codes::INVALID_ARGUMENT;
        }
        let slice = unsafe { std::slice::from_raw_parts(data, length as usize) };

        let result = with_network(|state| {
            if length as u32 > state.config.max_message_size {
                return Err(NetworkError::InvalidArgument(
                    "message exceeds max_message_size".into(),
                ));
            }
            state
                .session_router
                .send_from_leader(msg_type, slice, flags)
        });

        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in SendFromLeader");
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ForwardMessage(
    from: *const c_char,
    target: *const c_char,
    data: *const u8,
    length: i32,
    msg_type: u16,
    flags: u8,
) -> i32 {
    catch_unwind(|| {
        let from_str = match unsafe { c_str_to_string(from) } {
            Ok(s) => s,
            Err(e) => {
                set_network_error(&e);
                return e.error_code();
            }
        };
        if data.is_null() || length < 0 {
            set_error(
                error_codes::INVALID_ARGUMENT,
                "invalid data pointer or length",
            );
            return error_codes::INVALID_ARGUMENT;
        }
        let slice = unsafe { std::slice::from_raw_parts(data, length as usize) };
        let from_pid = PeerId::new(&from_str);

        let fwd_target = if target.is_null() {
            ForwardTarget::Broadcast
        } else {
            match unsafe { c_str_to_string(target) } {
                Ok(s) => ForwardTarget::ToPeer(PeerId::new(&s)),
                Err(e) => {
                    set_network_error(&e);
                    return e.error_code();
                }
            }
        };

        let result = with_network(|state| {
            if length as u32 > state.config.max_message_size {
                return Err(NetworkError::InvalidArgument(
                    "message exceeds max_message_size".into(),
                ));
            }
            state
                .session_router
                .forward_message(&from_pid, fwd_target, msg_type, flags, slice)
        });

        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in ForwardMessage");
        error_codes::INTERNAL_ERROR
    })
}

// --- callback registration ----------------------------------------------
#[unsafe(no_mangle)]
pub extern "C" fn RegisterReceiveCallback(callback: Option<ReceiveCallback>) -> i32 {
    catch_unwind(|| {
        let result = with_network(|state| {
            state.callback.register_receive_callback(callback);
            Ok(())
        });
        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(
            error_codes::INTERNAL_ERROR,
            "panic in RegisterReceiveCallback",
        );
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn RegisterLeaderChangedCallback(callback: Option<LeaderChangedCallback>) -> i32 {
    catch_unwind(|| {
        let result = with_network(|state| {
            state.callback.register_leader_changed_callback(callback);
            Ok(())
        });
        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(
            error_codes::INTERNAL_ERROR,
            "panic in RegisterLeaderChangedCallback",
        );
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn RegisterPeerStatusCallback(callback: Option<PeerStatusCallback>) -> i32 {
    catch_unwind(|| {
        let result = with_network(|state| {
            state.callback.register_peer_status_callback(callback);
            Ok(())
        });
        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(
            error_codes::INTERNAL_ERROR,
            "panic in RegisterPeerStatusCallback",
        );
        error_codes::INTERNAL_ERROR
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn RegisterConnectionResultCallback(
    callback: Option<ConnectionResultCallback>,
) -> i32 {
    catch_unwind(|| {
        let result = with_network(|state| {
            state.callback.register_connection_result_callback(callback);
            Ok(())
        });
        match result {
            Ok(()) => error_codes::OK,
            Err(e) => {
                set_network_error(&e);
                e.error_code()
            }
        }
    })
    .unwrap_or_else(|_| {
        set_error(
            error_codes::INTERNAL_ERROR,
            "panic in RegisterConnectionResultCallback",
        );
        error_codes::INTERNAL_ERROR
    })
}

// --- utility ------------------------------------------------------------
#[unsafe(no_mangle)]
pub extern "C" fn EnableLogging(enable: u8) -> i32 {
    catch_unwind(|| {
        if enable != 0 {
            #[cfg(feature = "logging")]
            {
                use std::sync::Once;

                static INIT: Once = Once::new();
                INIT.call_once(|| {
                    tracing_subscriber::fmt()
                        .with_ansi(false)
                        .with_env_filter(
                            tracing_subscriber::EnvFilter::from_default_env()
                                .add_directive(tracing::Level::INFO.into()),
                        )
                        .init();
                });
            }
        }
        error_codes::OK
    })
    .unwrap_or_else(|_| {
        set_error(error_codes::INTERNAL_ERROR, "panic in EnableLogging");
        error_codes::INTERNAL_ERROR
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // CString imported when needed by individual tests

    #[test]
    fn test_c_str_to_string_null() {
        let err = unsafe { c_str_to_string(std::ptr::null()) };
        assert!(matches!(err, Err(NetworkError::InvalidArgument(_))));
    }

    #[test]
    fn test_return_string_and_last_error() {
        let ptr = return_string("hello".into());
        let s = unsafe { CStr::from_ptr(ptr).to_str().unwrap() };
        assert_eq!(s, "hello");

        set_error(123, "oops");
        let guard = LAST_ERROR.lock();
        let pair = guard.as_ref().unwrap();
        assert_eq!(pair.0, 123);
        assert_eq!(pair.1, "oops");
    }
}
