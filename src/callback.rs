//! Callback management for notifying C clients about network events.
//! Provides thread‑safe registration and asynchronous dispatch.

use std::ffi::{CString, c_char};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::types::PeerId;

// Function pointer types exposed to the C side via FFI.
// These are all `unsafe extern "C"` because we call them
// across the language boundary.
pub type ConnectionResultCallback = unsafe extern "C" fn(*const c_char, u8, i32);
pub type LeaderChangedCallback = unsafe extern "C" fn(*const c_char);
pub type PeerStatusCallback = unsafe extern "C" fn(*const c_char, i32);
pub type ReceiveCallback = unsafe extern "C" fn(*const c_char, *const u8, i32, u16, u8);

/// Internal events that can be emitted by the Rust code and
/// forwarded to the registered callbacks.
///
/// Each variant carries the data necessary to perform the FFI
/// call later on the callback thread.
#[derive(Debug)]
pub enum CallbackEvent {
    ConnectionResult {
        addr: String,
        success: bool,
        error_code: i32,
    },
    LeaderChanged {
        leader_id: Option<String>,
    },
    PeerStatusChanged {
        peer_id: String,
        status: i32,
    },
    Received {
        peer_id: String,
        data: Bytes,
        msg_type: u16,
        flags: u8,
    },
}

/// Manages the lifecycle and invocation of callbacks that are
/// registered from the FFI boundary. Events are sent from
/// various parts of the networking stack and processed on a
/// dedicated thread to avoid blocking async tasks.
///
/// The manager holds optional callback pointers behind `Mutex`es
/// so that registrations can happen at any time without races.
pub struct CallbackManager {
    // optional FFI callback pointers protected by mutexes
    connection_result_callback: Arc<Mutex<Option<ConnectionResultCallback>>>,
    leader_changed_callback: Arc<Mutex<Option<LeaderChangedCallback>>>,
    peer_status_callback: Arc<Mutex<Option<PeerStatusCallback>>>,
    receive_callback: Arc<Mutex<Option<ReceiveCallback>>>,

    // handle of the thread running `callback_loop`
    callback_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    // channel used to send events to the callback thread
    event_tx: Mutex<Option<mpsc::Sender<CallbackEvent>>>,
    // shared flag used to signal shutdown of the thread
    shutdown: Arc<AtomicBool>,
}

impl CallbackManager {
    /// Create a new manager, spawn the background thread and
    /// give it ownership of the receiver side of the channel.
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(1024);

        // initially no callbacks registered
        let connection_result_cb: Arc<Mutex<Option<ConnectionResultCallback>>> =
            Arc::new(Mutex::new(None));
        let leader_changed_cb: Arc<Mutex<Option<LeaderChangedCallback>>> =
            Arc::new(Mutex::new(None));
        let peer_status_cb: Arc<Mutex<Option<PeerStatusCallback>>> = Arc::new(Mutex::new(None));
        let receive_cb: Arc<Mutex<Option<ReceiveCallback>>> = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));

        // clone arcs for the thread
        let thread_connection_result = Arc::clone(&connection_result_cb);
        let thread_leader_changed = Arc::clone(&leader_changed_cb);
        let thread_peer_status = Arc::clone(&peer_status_cb);
        let thread_receive = Arc::clone(&receive_cb);
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            // the loop that runs on the dedicated thread
            Self::callback_loop(
                event_rx,
                thread_connection_result,
                thread_leader_changed,
                thread_peer_status,
                thread_receive,
                thread_shutdown,
            );
        });

        Self {
            connection_result_callback: connection_result_cb,
            leader_changed_callback: leader_changed_cb,
            peer_status_callback: peer_status_cb,
            receive_callback: receive_cb,
            callback_thread: Mutex::new(Some(handle)),
            event_tx: Mutex::new(Some(event_tx)),
            shutdown,
        }
    }

    /// Blocking loop executed on the helper thread. We pull
    /// events out of the channel and dispatch them until either
    /// the sender is dropped or a shutdown flag is set.
    fn callback_loop(
        mut event_rx: mpsc::Receiver<CallbackEvent>,
        connection_result_cb: Arc<Mutex<Option<ConnectionResultCallback>>>,
        leader_changed_cb: Arc<Mutex<Option<LeaderChangedCallback>>>,
        peer_status_cb: Arc<Mutex<Option<PeerStatusCallback>>>,
        receive_cb: Arc<Mutex<Option<ReceiveCallback>>>,
        shutdown: Arc<AtomicBool>,
    ) {
        while !shutdown.load(Ordering::Relaxed) {
            match event_rx.blocking_recv() {
                Some(event) => {
                    Self::dispatch_event(
                        &event,
                        &connection_result_cb,
                        &leader_changed_cb,
                        &peer_status_cb,
                        &receive_cb,
                    );
                }
                None => break, // channel closed
            }
        }
    }

    /// Convert a `CallbackEvent` into a concrete FFI call if a
    /// callback has been registered. Each branch takes the lock
    /// briefly, clones or constructs a `CString` and invokes the
    /// unsafe function pointer.
    fn dispatch_event(
        event: &CallbackEvent,
        connection_result_cb: &Arc<Mutex<Option<ConnectionResultCallback>>>,
        leader_changed_cb: &Arc<Mutex<Option<LeaderChangedCallback>>>,
        peer_status_cb: &Arc<Mutex<Option<PeerStatusCallback>>>,
        receive_cb: &Arc<Mutex<Option<ReceiveCallback>>>,
    ) {
        match event {
            CallbackEvent::ConnectionResult {
                addr,
                success,
                error_code,
            } => {
                let cb = *connection_result_cb.lock();
                if let Some(cb) = cb
                    && let Ok(c_addr) = CString::new(addr.as_str())
                {
                    unsafe {
                        cb(c_addr.as_ptr(), u8::from(*success), *error_code);
                    }
                }
            }
            CallbackEvent::LeaderChanged { leader_id } => {
                let cb = *leader_changed_cb.lock();
                if let Some(cb) = cb {
                    let leader_str = leader_id.as_deref().unwrap_or("");
                    if let Ok(c_leader) = CString::new(leader_str) {
                        unsafe {
                            cb(c_leader.as_ptr());
                        }
                    }
                }
            }
            CallbackEvent::PeerStatusChanged { peer_id, status } => {
                let cb = *peer_status_cb.lock();
                if let Some(cb) = cb
                    && let Ok(c_peer) = CString::new(peer_id.as_str())
                {
                    unsafe {
                        cb(c_peer.as_ptr(), *status);
                    }
                }
            }
            CallbackEvent::Received {
                peer_id,
                data,
                msg_type,
                flags,
            } => {
                let cb = *receive_cb.lock();
                if let Some(cb) = cb
                    && let Ok(c_peer) = CString::new(peer_id.as_str())
                {
                    unsafe {
                        cb(
                            c_peer.as_ptr(),
                            data.as_ptr(),
                            data.len() as i32,
                            *msg_type,
                            *flags,
                        );
                    }
                }
            }
        }
    }

    // ----- helper methods used by other modules --------------------------

    /// Try to enqueue an event; on full queue we drop and log.
    pub fn send_event(&self, event: CallbackEvent) {
        let guard = self.event_tx.lock();
        if let Some(tx) = guard.as_ref()
            && tx.try_send(event).is_err()
        {
            tracing::warn!("callback event queue full, dropping event");
        }
    }

    /// Helper method to send a connection result event.
    pub fn send_connection_result(&self, addr: &str, success: bool, error_code: i32) {
        self.send_event(CallbackEvent::ConnectionResult {
            addr: addr.to_owned(),
            success,
            error_code,
        });
    }

    /// Helper method to send a leader changed event.
    pub fn send_leader_changed(&self, leader_id: Option<&PeerId>) {
        self.send_event(CallbackEvent::LeaderChanged {
            leader_id: leader_id.map(|p| p.as_str().to_owned()),
        });
    }

    /// Helper method to send a peer status changed event.
    pub fn send_peer_status(&self, peer_id: &PeerId, status: i32) {
        self.send_event(CallbackEvent::PeerStatusChanged {
            peer_id: peer_id.as_str().to_owned(),
            status,
        });
    }

    /// Helper method to send a received message event.
    pub fn send_received(&self, peer_id: &PeerId, data: Bytes, msg_type: u16, flags: u8) {
        self.send_event(CallbackEvent::Received {
            peer_id: peer_id.as_str().to_owned(),
            data,
            msg_type,
            flags,
        });
    }

    // ----- registration API ----------------------------------------------

    /// Install or clear the connection result callback.
    pub fn register_connection_result_callback(&self, cb: Option<ConnectionResultCallback>) {
        *self.connection_result_callback.lock() = cb;
    }

    /// Install or clear the leader change callback.
    pub fn register_leader_changed_callback(&self, cb: Option<LeaderChangedCallback>) {
        *self.leader_changed_callback.lock() = cb;
    }

    /// Install or clear the peer status callback.
    pub fn register_peer_status_callback(&self, cb: Option<PeerStatusCallback>) {
        *self.peer_status_callback.lock() = cb;
    }

    /// Install or clear the receive callback.
    pub fn register_receive_callback(&self, cb: Option<ReceiveCallback>) {
        *self.receive_callback.lock() = cb;
    }

    // ----- shutdown helpers ----------------------------------------------

    /// Stop accepting new events and signal the worker thread to exit.
    pub fn drain_and_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.event_tx.lock().take(); // drop sender so loop ends
    }

    /// Wait for the background thread to terminate; log if it panicked.
    pub fn join_thread(&self) {
        if let Some(handle) = self.callback_thread.lock().take()
            && handle.join().is_err()
        {
            tracing::error!("callback thread panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::sync::atomic::{AtomicBool, Ordering};

    static CONNECTION_CALLED: AtomicBool = AtomicBool::new(false);
    static LEADER_CALLED: AtomicBool = AtomicBool::new(false);
    static STATUS_CALLED: AtomicBool = AtomicBool::new(false);
    static RECEIVE_CALLED: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn conn_cb(addr: *const c_char, success: u8, _code: i32) {
        let s = unsafe { CStr::from_ptr(addr) }.to_str().unwrap();
        if s == "foo" && success == 1 {
            CONNECTION_CALLED.store(true, Ordering::Relaxed);
        }
    }

    unsafe extern "C" fn leader_cb(id: *const c_char) {
        let _ = unsafe { CStr::from_ptr(id) }.to_str().unwrap();
        LEADER_CALLED.store(true, Ordering::Relaxed);
    }

    unsafe extern "C" fn status_cb(id: *const c_char, _status: i32) {
        let _ = unsafe { CStr::from_ptr(id) }.to_str().unwrap();
        STATUS_CALLED.store(true, Ordering::Relaxed);
    }

    unsafe extern "C" fn recv_cb(
        id: *const c_char,
        _data: *const u8,
        _len: i32,
        _msg_type: u16,
        _flags: u8,
    ) {
        let _ = unsafe { CStr::from_ptr(id) }.to_str().unwrap();
        RECEIVE_CALLED.store(true, Ordering::Relaxed);
    }

    #[test]
    fn test_registration_and_delivery() {
        let mgr = CallbackManager::new();
        mgr.register_connection_result_callback(Some(conn_cb));
        mgr.register_leader_changed_callback(Some(leader_cb));
        mgr.register_peer_status_callback(Some(status_cb));
        mgr.register_receive_callback(Some(recv_cb));

        mgr.send_connection_result("foo", true, 0);
        mgr.send_leader_changed(Some(&PeerId::new("x")));
        mgr.send_peer_status(&PeerId::new("y"), 5);
        mgr.send_received(&PeerId::new("z"), Bytes::from(&b"hi"[..]), 0x100, 3);

        // give the callback thread a moment to dequeue and dispatch
        std::thread::sleep(std::time::Duration::from_millis(10));

        // shut down thread so that all events are flushed
        mgr.drain_and_shutdown();
        mgr.join_thread();

        assert!(CONNECTION_CALLED.load(Ordering::Relaxed));
        assert!(LEADER_CALLED.load(Ordering::Relaxed));
        assert!(STATUS_CALLED.load(Ordering::Relaxed));
        assert!(RECEIVE_CALLED.load(Ordering::Relaxed));
    }

    #[test]
    fn test_unregister_and_shutdown() {
        let mgr = CallbackManager::new();
        mgr.register_connection_result_callback(None);
        mgr.register_leader_changed_callback(None);
        mgr.register_peer_status_callback(None);
        mgr.register_receive_callback(None);

        // sending events with no callbacks should not panic
        mgr.send_event(CallbackEvent::ConnectionResult {
            addr: "a".into(),
            success: false,
            error_code: 1,
        });

        mgr.drain_and_shutdown();
        mgr.join_thread();
    }
}
