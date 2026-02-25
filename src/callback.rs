use std::ffi::{CString, c_char};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::types::PeerId;

pub type ReceiveCallback = unsafe extern "C" fn(*const c_char, *const u8, i32, u16, u8);
pub type LeaderChangedCallback = unsafe extern "C" fn(*const c_char);
pub type PeerStatusCallback = unsafe extern "C" fn(*const c_char, i32);
pub type ConnectionResultCallback = unsafe extern "C" fn(*const c_char, u8, i32);

#[derive(Debug)]
pub enum CallbackEvent {
    MessageReceived {
        peer_id: String,
        data: Bytes,
        msg_type: u16,
        flags: u8,
    },
    LeaderChanged {
        leader_id: Option<String>,
    },
    PeerStatusChanged {
        peer_id: String,
        status: i32,
    },
    ConnectionResult {
        addr: String,
        success: bool,
        error_code: i32,
    },
}

pub struct CallbackManager {
    receive_callback: Arc<Mutex<Option<ReceiveCallback>>>,
    leader_changed_callback: Arc<Mutex<Option<LeaderChangedCallback>>>,
    peer_status_callback: Arc<Mutex<Option<PeerStatusCallback>>>,
    connection_result_callback: Arc<Mutex<Option<ConnectionResultCallback>>>,
    event_tx: Mutex<Option<mpsc::Sender<CallbackEvent>>>,
    callback_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    shutdown: Arc<AtomicBool>,
}

impl CallbackManager {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(1024);

        let receive_cb: Arc<Mutex<Option<ReceiveCallback>>> = Arc::new(Mutex::new(None));
        let leader_cb: Arc<Mutex<Option<LeaderChangedCallback>>> = Arc::new(Mutex::new(None));
        let peer_status_cb: Arc<Mutex<Option<PeerStatusCallback>>> = Arc::new(Mutex::new(None));
        let conn_result_cb: Arc<Mutex<Option<ConnectionResultCallback>>> =
            Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_receive = Arc::clone(&receive_cb);
        let thread_leader = Arc::clone(&leader_cb);
        let thread_peer_status = Arc::clone(&peer_status_cb);
        let thread_conn_result = Arc::clone(&conn_result_cb);
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            Self::callback_loop(
                event_rx,
                thread_receive,
                thread_leader,
                thread_peer_status,
                thread_conn_result,
                thread_shutdown,
            );
        });

        Self {
            receive_callback: receive_cb,
            leader_changed_callback: leader_cb,
            peer_status_callback: peer_status_cb,
            connection_result_callback: conn_result_cb,
            event_tx: Mutex::new(Some(event_tx)),
            callback_thread: Mutex::new(Some(handle)),
            shutdown,
        }
    }

    fn callback_loop(
        mut event_rx: mpsc::Receiver<CallbackEvent>,
        receive_cb: Arc<Mutex<Option<ReceiveCallback>>>,
        leader_cb: Arc<Mutex<Option<LeaderChangedCallback>>>,
        peer_status_cb: Arc<Mutex<Option<PeerStatusCallback>>>,
        conn_result_cb: Arc<Mutex<Option<ConnectionResultCallback>>>,
        shutdown: Arc<AtomicBool>,
    ) {
        while !shutdown.load(Ordering::Relaxed) {
            match event_rx.blocking_recv() {
                Some(event) => {
                    Self::dispatch_event(
                        &event,
                        &receive_cb,
                        &leader_cb,
                        &peer_status_cb,
                        &conn_result_cb,
                    );
                }
                None => break,
            }
        }
    }

    fn dispatch_event(
        event: &CallbackEvent,
        receive_cb: &Arc<Mutex<Option<ReceiveCallback>>>,
        leader_cb: &Arc<Mutex<Option<LeaderChangedCallback>>>,
        peer_status_cb: &Arc<Mutex<Option<PeerStatusCallback>>>,
        conn_result_cb: &Arc<Mutex<Option<ConnectionResultCallback>>>,
    ) {
        match event {
            CallbackEvent::MessageReceived {
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
            CallbackEvent::LeaderChanged { leader_id } => {
                let cb = *leader_cb.lock();
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
            CallbackEvent::ConnectionResult {
                addr,
                success,
                error_code,
            } => {
                let cb = *conn_result_cb.lock();
                if let Some(cb) = cb
                    && let Ok(c_addr) = CString::new(addr.as_str())
                {
                    unsafe {
                        cb(c_addr.as_ptr(), u8::from(*success), *error_code);
                    }
                }
            }
        }
    }

    pub fn send_event(&self, event: CallbackEvent) {
        let guard = self.event_tx.lock();
        if let Some(tx) = guard.as_ref()
            && tx.try_send(event).is_err()
        {
            tracing::warn!("callback event queue full, dropping event");
        }
    }

    pub fn send_message_received(&self, peer_id: &PeerId, data: Bytes, msg_type: u16, flags: u8) {
        self.send_event(CallbackEvent::MessageReceived {
            peer_id: peer_id.as_str().to_owned(),
            data,
            msg_type,
            flags,
        });
    }

    pub fn send_leader_changed(&self, leader_id: Option<&PeerId>) {
        self.send_event(CallbackEvent::LeaderChanged {
            leader_id: leader_id.map(|p| p.as_str().to_owned()),
        });
    }

    pub fn send_peer_status(&self, peer_id: &PeerId, status: i32) {
        self.send_event(CallbackEvent::PeerStatusChanged {
            peer_id: peer_id.as_str().to_owned(),
            status,
        });
    }

    pub fn send_connection_result(&self, addr: &str, success: bool, error_code: i32) {
        self.send_event(CallbackEvent::ConnectionResult {
            addr: addr.to_owned(),
            success,
            error_code,
        });
    }

    pub fn register_receive_callback(&self, cb: Option<ReceiveCallback>) {
        *self.receive_callback.lock() = cb;
    }

    pub fn register_leader_changed_callback(&self, cb: Option<LeaderChangedCallback>) {
        *self.leader_changed_callback.lock() = cb;
    }

    pub fn register_peer_status_callback(&self, cb: Option<PeerStatusCallback>) {
        *self.peer_status_callback.lock() = cb;
    }

    pub fn register_connection_result_callback(&self, cb: Option<ConnectionResultCallback>) {
        *self.connection_result_callback.lock() = cb;
    }

    pub fn drain_and_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.event_tx.lock().take();
    }

    pub fn join_thread(&self) {
        if let Some(handle) = self.callback_thread.lock().take()
            && handle.join().is_err()
        {
            tracing::error!("callback thread panicked");
        }
    }
}
