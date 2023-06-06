use crate::networking::socket::{schedule_descriptor_read, MutinySocketDescriptor, ReadDescriptor};
use crate::peermanager::PeerManager;
use crate::utils;
use crate::{error::MutinyError, networking::proxy::Proxy};
use bitcoin::hashes::hex::ToHex;
use crossbeam_channel::{unbounded, Receiver, Sender};
use futures::lock::Mutex;
use futures::{pin_mut, select, FutureExt};
use gloo_net::websocket::Message;
use lightning::{ln::peer_handler, log_debug, log_error, log_info, util::logger::Logger};
use lightning::{ln::peer_handler::SocketDescriptor, log_trace};
use ln_websocket_proxy::MutinyProxyCommand;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
const PUBKEY_BYTES_LEN: usize = 33;

pub type SubSocketMap = Arc<Mutex<HashMap<Vec<u8>, (SubWsSocketDescriptor, Sender<Message>)>>>;

pub struct WsTcpSocketDescriptor {
    conn: Arc<dyn Proxy>,
    id: u64,
}

impl WsTcpSocketDescriptor {
    pub fn new(conn: Arc<dyn Proxy>) -> Self {
        let id = ID_COUNTER.fetch_add(1, Ordering::AcqRel);
        Self { conn, id }
    }
}

impl ReadDescriptor for WsTcpSocketDescriptor {
    async fn read(&self) -> Option<Result<Vec<u8>, MutinyError>> {
        match self.conn.read().await {
            Some(Ok(Message::Bytes(b))) => Some(Ok(b)),
            Some(Ok(Message::Text(_))) => {
                // Ignoring text messages sent through tcp socket
                None
            }
            Some(Err(_)) => Some(Err(MutinyError::ConnectionFailed)),
            None => None,
        }
    }
}

unsafe impl Send for WsTcpSocketDescriptor {}
unsafe impl Sync for WsTcpSocketDescriptor {}

impl peer_handler::SocketDescriptor for WsTcpSocketDescriptor {
    fn send_data(&mut self, data: &[u8], _resume_read: bool) -> usize {
        let vec = Vec::from(data);
        self.conn.send(Message::Bytes(vec));
        data.len()
    }

    fn disconnect_socket(&mut self) {
        let cloned = self.conn.clone();
        utils::spawn(async move {
            cloned.close().await;
        });
    }
}
impl Clone for WsTcpSocketDescriptor {
    fn clone(&self) -> Self {
        Self {
            conn: Arc::clone(&self.conn),
            id: self.id,
        }
    }
}
impl Eq for WsTcpSocketDescriptor {}
impl PartialEq for WsTcpSocketDescriptor {
    fn eq(&self, o: &Self) -> bool {
        self.id == o.id
    }
}
impl Hash for WsTcpSocketDescriptor {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl std::fmt::Debug for WsTcpSocketDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "({})", self.id)
    }
}

pub struct MultiWsSocketDescriptor {
    /// Once `conn` has been set, it MUST NOT be unset.
    /// This is typically set via `connect`
    conn: Option<Arc<dyn Proxy>>,
    read_from_sub_socket: Receiver<Message>,
    send_to_multi_socket: Sender<Message>,
    socket_map: SubSocketMap,
    peer_manager: Arc<dyn PeerManager>,
    our_peer_pubkey: Vec<u8>,
    connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    logger: Arc<dyn Logger>,
}

impl MultiWsSocketDescriptor {
    /// Set up a new MultiWsSocketDescriptor that prepares for connections.
    /// Passing in the Proxy connection is required before listening can begin.
    pub fn new(
        peer_manager: Arc<dyn PeerManager>,
        our_peer_pubkey: Vec<u8>,
        stop: Arc<AtomicBool>,
        logger: Arc<dyn Logger>,
    ) -> Self {
        log_info!(logger, "setting up multi websocket descriptor");

        let (send_to_multi_socket, read_from_sub_socket): (Sender<Message>, Receiver<Message>) =
            unbounded();

        let socket_map: SubSocketMap = Arc::new(Mutex::new(HashMap::new()));

        let connected = Arc::new(AtomicBool::new(false));
        Self {
            conn: None,
            send_to_multi_socket,
            read_from_sub_socket,
            socket_map,
            peer_manager,
            our_peer_pubkey,
            stop,
            connected,
            logger,
        }
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn attempt_keep_alive(&self) {
        if let Some(conn) = &self.conn {
            conn.send(Message::Text(
                serde_json::to_string(&MutinyProxyCommand::Ping {}).unwrap(),
            ));
        }
    }

    pub async fn connect(&mut self, conn: Arc<dyn Proxy>) {
        let mut socket_map = self.socket_map.lock().await;
        log_trace!(self.logger, "connecting new multi websocket descriptor");
        // if reconnecting master socket, disconnect and clear all subsockets
        for (_id, (subsocket, _sender)) in socket_map.iter_mut() {
            // tell the subsocket to stop processing
            // and ldk to disconnect that peer
            subsocket.disconnect_socket();
            self.peer_manager
                .socket_disconnected(&mut MutinySocketDescriptor::Mutiny(subsocket.clone()));
        }

        socket_map.clear();
        // Once `conn` has been set, it must not be unset.
        // It should not panic if it unset, but it will ruin the reconnection logic
        // and the expectations around calling `listen`.
        self.conn = Some(conn);
        self.connected.store(true, Ordering::Relaxed);

        self.listen();
    }

    pub async fn create_new_subsocket(&self, id: Vec<u8>) -> SubWsSocketDescriptor {
        let (send_to_sub_socket, read_from_multi_socket): (Sender<Message>, Receiver<Message>) =
            unbounded();
        create_new_subsocket(
            self.socket_map.clone(),
            self.send_to_multi_socket.clone(),
            send_to_sub_socket,
            read_from_multi_socket,
            id,
            self.our_peer_pubkey.clone(),
            self.logger.clone(),
        )
        .await
    }

    /// Listen starts listening to the socket.
    /// This should only be called if `connect` has created `Some(conn)`
    fn listen(&self) {
        // sanity check to make sure conn has been set first
        if self.conn.is_none() {
            self.connected.store(false, Ordering::Relaxed);
            return;
        }

        // This first part will take in messages from the websocket connection
        // to the proxy and decide what to do with them. If it is a binary message
        // then it is a message from one mutiny peer to another with the first bytes
        // being the pubkey it is for. In that case, it will find the subsocket
        // for the pubkey it is for and send the rest of the bytes to it.
        //
        // The websocket proxy may also send commands to the multi websocket descriptor.
        // A disconnection message indicates that a subsocket descriptor needs to be
        // closed but the underlying connection should stay open. This indicates that
        // the other peer went away or there was an issue connecting / sending to them.
        let conn_copy = self.conn.clone().unwrap().clone();
        let socket_map_copy = self.socket_map.clone();
        let send_to_multi_socket_copy = self.send_to_multi_socket.clone();
        let peer_manager_copy = self.peer_manager.clone();
        let connected_copy = self.connected.clone();
        let our_peer_pubkey_copy = self.our_peer_pubkey.clone();
        let stop_copy = self.stop.clone();
        let logger_copy = self.logger.clone();
        log_trace!(self.logger, "spawning multi socket connection reader");
        utils::spawn(async move {
            loop {
                let mut read_fut = conn_copy.read().fuse();
                let delay_fut = Box::pin(utils::sleep(1_000)).fuse();
                pin_mut!(delay_fut);
                select! {
                    msg_option = read_fut => {
                        match msg_option {
                            Some(Ok(msg)) => {
                                handle_incoming_msg(
                                    msg,
                                    socket_map_copy.clone(),
                                    peer_manager_copy.clone(),
                                    send_to_multi_socket_copy.clone(),
                                    our_peer_pubkey_copy.clone(),
                                    logger_copy.clone(),
                                    stop_copy.clone(),
                                ).await;
                                }
                            Some(Err(e)) => {
                                log_trace!(logger_copy, "could not read from proxy connection: {e}");
                                break;
                            }
                            None => {
                                log_trace!(logger_copy, "nothing from the future, ignoring...");

                            }
                        }
                    }
                    _ = delay_fut => {
                        if stop_copy.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                }
            }
            log_trace!(logger_copy, "leaving multi socket connection reader");
            connected_copy.store(false, Ordering::Relaxed)
        });

        let read_channel_copy = self.read_from_sub_socket.clone();
        let conn_copy_send = self.conn.clone().unwrap().clone();
        let connected_copy_send = self.connected.clone();
        let logger_send_copy = self.logger.clone();
        log_trace!(self.logger, "spawning multi socket channel reader");
        utils::spawn(async move {
            loop {
                if let Ok(msg) = read_channel_copy.try_recv() {
                    log_trace!(
                        logger_send_copy,
                        "multi socket channel reader sending data to proxy"
                    );
                    conn_copy_send.send(msg)
                }
                if !connected_copy_send.load(Ordering::Relaxed) {
                    break;
                }
                utils::sleep(50).await;
            }
            log_trace!(logger_send_copy, "leaving multi socket channel reader");
        });
    }
}

async fn handle_incoming_msg(
    msg: Message,
    socket_map: SubSocketMap,
    peer_manager: Arc<dyn PeerManager>,
    send_to_multi_socket: Sender<Message>,
    our_peer_pubkey: Vec<u8>,
    logger: Arc<dyn Logger>,
    stop: Arc<AtomicBool>,
) {
    match msg {
        Message::Text(msg) => {
            // This is a text command from the server. Parse the type
            // of command it is and act accordingly.
            // parse and implement subsocket disconnections.
            // Right now subsocket is very tied to a specific node,
            // later we should share a single connection amongst all pubkeys and in
            // which case "to" will be important to parse.
            let command: MutinyProxyCommand = match serde_json::from_str(&msg) {
                Ok(c) => c,
                Err(e) => {
                    log_error!(
                        logger,
                        "couldn't parse text command from proxy, ignoring: {e}"
                    );
                    return;
                }
            };
            match command {
                MutinyProxyCommand::Disconnect { to: _to, from } => {
                    let mut locked_socket_map = socket_map.lock().await;
                    match locked_socket_map.get_mut(&from) {
                        Some((subsocket, _sender)) => {
                            // if we got told by server to disconnect then stop
                            // reading from the socket and tell LDK that the socket
                            // is disconnected.
                            log_debug!(
                                logger,
                                "was told by server to disconnect subsocket connection with {}",
                                from.to_hex()
                            );
                            subsocket.stop_reading();
                            peer_manager.socket_disconnected(&mut MutinySocketDescriptor::Mutiny(
                                subsocket.clone(),
                            ));
                            locked_socket_map.remove(&from);
                        }
                        None => {
                            log_error!(
                                logger,
                                "tried to disconnect a subsocket that doesn't exist..."
                            );
                        }
                    }
                }
                MutinyProxyCommand::Ping => {
                    // Ignore, we send to them
                }
            };
        }
        Message::Bytes(msg) => {
            // This is a mutiny to mutiny connection with pubkey + bytes
            // as the binary message. Parse the msg and see which pubkey
            // it belongs to.
            if msg.len() < PUBKEY_BYTES_LEN {
                log_error!(logger, "msg not long enough to have pubkey, ignoring...");
                return;
            }
            let (id_bytes, message_bytes) = msg.split_at(PUBKEY_BYTES_LEN);

            // now send that data to the right subsocket;
            let socket_lock = socket_map.lock().await;
            let found_subsocket = socket_lock.get(id_bytes);
            match found_subsocket {
                Some((_subsocket, sender)) => {
                    match sender.send(Message::Bytes(message_bytes.to_vec())) {
                        Ok(_) => {}
                        Err(e) => log_error!(logger, "error sending msg to channel: {}", e),
                    };
                }
                None => {
                    drop(socket_lock);

                    // create a new subsocket and pass it to peer_manager
                    log_trace!(
                        logger,
                        "no connection found for socket address, creating new: {:?}",
                        id_bytes
                    );
                    let (send_to_sub_socket, read_from_multi_socket): (
                        Sender<Message>,
                        Receiver<Message>,
                    ) = unbounded();
                    let mut inbound_subsocket = MutinySocketDescriptor::Mutiny(
                        create_new_subsocket(
                            socket_map.clone(),
                            send_to_multi_socket.clone(),
                            send_to_sub_socket.clone(),
                            read_from_multi_socket,
                            id_bytes.to_vec(),
                            our_peer_pubkey.clone(),
                            logger.clone(),
                        )
                        .await,
                    );
                    log_trace!(logger, "created new subsocket: {:?}", id_bytes);
                    match peer_manager.new_inbound_connection(inbound_subsocket.clone(), None) {
                        Ok(_) => {
                            log_trace!(
                                logger,
                                "gave new subsocket to peer manager: {:?}",
                                id_bytes
                            );
                            schedule_descriptor_read(
                                inbound_subsocket,
                                peer_manager.clone(),
                                logger.clone(),
                                stop.clone(),
                            );

                            // now that we have the inbound connection, send the original
                            // message to our new subsocket descriptor
                            match send_to_sub_socket.send(Message::Bytes(message_bytes.to_vec())) {
                                Ok(_) => {
                                    log_trace!(
                                        logger,
                                        "sent incoming message to new subsocket channel: {:?}",
                                        id_bytes
                                    )
                                }
                                Err(e) => {
                                    log_error!(logger, "error sending msg to channel: {}", e)
                                }
                            };
                        }
                        Err(_) => {
                            log_error!(
                                logger,
                                "peer manager could not handle subsocket for: {:?}, deleting...",
                                id_bytes
                            );
                            let mut locked_socket_map = socket_map.lock().await;
                            inbound_subsocket.disconnect_socket();
                            peer_manager.socket_disconnected(&mut inbound_subsocket);
                            locked_socket_map.remove(id_bytes);
                        }
                    };
                }
            };
        }
    }
}

pub async fn create_new_subsocket(
    socket_map: SubSocketMap,
    send_to_multi_socket: Sender<Message>,
    send_to_sub_socket: Sender<Message>,
    read_from_multi_socket: Receiver<Message>,
    peer_pubkey: Vec<u8>,
    our_pubkey: Vec<u8>,
    logger: Arc<dyn Logger>,
) -> SubWsSocketDescriptor {
    let new_subsocket = SubWsSocketDescriptor::new(
        send_to_multi_socket,
        read_from_multi_socket,
        peer_pubkey.clone(),
        our_pubkey,
        logger,
    );

    socket_map
        .lock()
        .await
        .insert(peer_pubkey, (new_subsocket.clone(), send_to_sub_socket));

    new_subsocket
}

pub struct SubWsSocketDescriptor {
    send_channel: Sender<Message>,
    read_channel: Receiver<Message>,
    peer_pubkey_bytes: Vec<u8>,
    our_pubkey_bytes: Vec<u8>,
    id: u64,
    stop: Arc<AtomicBool>,
    logger: Arc<dyn Logger>,
}
impl SubWsSocketDescriptor {
    pub fn new(
        send_channel: Sender<Message>,
        read_channel: Receiver<Message>,
        peer_pubkey_bytes: Vec<u8>,
        our_pubkey_bytes: Vec<u8>,
        logger: Arc<dyn Logger>,
    ) -> Self {
        let id = ID_COUNTER.fetch_add(1, Ordering::AcqRel);
        Self {
            read_channel,
            send_channel,
            peer_pubkey_bytes,
            our_pubkey_bytes,
            id,
            stop: Arc::new(AtomicBool::new(false)),
            logger,
        }
    }

    pub fn stop_reading(&self) {
        self.stop.store(true, Ordering::Relaxed)
    }
}

impl ReadDescriptor for SubWsSocketDescriptor {
    async fn read(&self) -> Option<Result<Vec<u8>, MutinyError>> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                log_trace!(self.logger, "stopping subsocket channel reader");
                return Some(Err(MutinyError::ConnectionFailed));
            }
            if let Ok(Message::Bytes(b)) = self.read_channel.try_recv() {
                return Some(Ok(b));
            }
            utils::sleep(50).await;
        }
    }
}

impl peer_handler::SocketDescriptor for SubWsSocketDescriptor {
    fn send_data(&mut self, data: &[u8], _resume_read: bool) -> usize {
        if self.stop.load(Ordering::Relaxed) {
            log_trace!(
                self.logger,
                "ignoring request to send down stopped subsocket"
            );
            return 0;
        }

        let mut addr_prefix = self.peer_pubkey_bytes.to_vec();
        let mut vec = Vec::from(data);
        addr_prefix.append(&mut vec);
        let res = self.send_channel.send(Message::Bytes(addr_prefix));
        if res.is_err() {
            0
        } else {
            data.len()
        }
    }

    fn disconnect_socket(&mut self) {
        log_trace!(self.logger, "disconnecting socket from LDK");
        let res = self.send_channel.send(Message::Text(
            serde_json::to_string(&MutinyProxyCommand::Disconnect {
                to: self.peer_pubkey_bytes.clone(),
                from: self.our_pubkey_bytes.clone(),
            })
            .unwrap(),
        ));
        if res.is_err() {
            log_error!(
                self.logger,
                "tried to send disconnect message to proxy but failed.."
            )
        }
        self.stop.store(true, Ordering::Relaxed)
    }
}

unsafe impl Send for SubWsSocketDescriptor {}
unsafe impl Sync for SubWsSocketDescriptor {}

impl Clone for SubWsSocketDescriptor {
    fn clone(&self) -> Self {
        Self {
            read_channel: self.read_channel.clone(),
            send_channel: self.send_channel.clone(),
            peer_pubkey_bytes: self.peer_pubkey_bytes.clone(),
            our_pubkey_bytes: self.our_pubkey_bytes.clone(),
            id: self.id,
            stop: self.stop.clone(),
            logger: self.logger.clone(),
        }
    }
}
impl Eq for SubWsSocketDescriptor {}
impl PartialEq for SubWsSocketDescriptor {
    fn eq(&self, o: &Self) -> bool {
        self.id == o.id
    }
}
impl Hash for SubWsSocketDescriptor {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl std::fmt::Debug for SubWsSocketDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "({})", self.id)
    }
}

#[cfg(test)]
mod tests {
    use crate::{logging::TestLogger, networking::proxy::MockProxy};

    use wasm_bindgen_test::{wasm_bindgen_test as test, wasm_bindgen_test_configure};

    use super::MutinySocketDescriptor;
    use crate::networking::ws_socket::create_new_subsocket;
    use crate::networking::ws_socket::SubSocketMap;
    use crate::networking::ws_socket::WsTcpSocketDescriptor;
    use bitcoin::secp256k1::PublicKey;
    use crossbeam_channel::{unbounded, Receiver, Sender};
    use futures::lock::Mutex;
    use gloo_net::websocket::Message;
    use lightning::util::ser::Writeable;
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::Arc;

    wasm_bindgen_test_configure!(run_in_browser);

    const PEER_PUBKEY: &str = "02e6642fd69bd211f93f7f1f36ca51a26a5290eb2dd1b0d8279a87bb0d480c8443";

    const OTHER_PEER_PUBKEY: &str =
        "03b661d965727a0751bd876efe3c826f89d5056f98501924222abd552bc2ba0ab1";

    #[test]
    async fn test_eq_for_ws_socket_descriptor() {
        // Test ne and eq for WsTcpSocketDescriptor
        let mock_proxy = Arc::new(MockProxy::new());
        let tcp_ws = MutinySocketDescriptor::Tcp(WsTcpSocketDescriptor::new(mock_proxy));

        let mock_proxy_2 = Arc::new(MockProxy::new());
        let tcp_ws_2 = MutinySocketDescriptor::Tcp(WsTcpSocketDescriptor::new(mock_proxy_2));
        assert_ne!(tcp_ws, tcp_ws_2);

        let mock_proxy_3 = Arc::new(MockProxy::new());
        let tcp_ws_3 = MutinySocketDescriptor::Tcp(WsTcpSocketDescriptor::new(mock_proxy_3));
        assert_eq!(tcp_ws_3.clone(), tcp_ws_3);

        // Test ne and eq for WsTcpSocketDescriptor
        let (send_to_multi_socket, _): (Sender<Message>, Receiver<Message>) = unbounded();

        let socket_map: SubSocketMap = Arc::new(Mutex::new(HashMap::new()));
        let (send_to_sub_socket, read_from_multi_socket): (Sender<Message>, Receiver<Message>) =
            unbounded();
        let sub_ws_socket = create_new_subsocket(
            socket_map.clone(),
            send_to_multi_socket.clone(),
            send_to_sub_socket,
            read_from_multi_socket,
            PublicKey::from_str(OTHER_PEER_PUBKEY).unwrap().encode(),
            PublicKey::from_str(PEER_PUBKEY).unwrap().encode(),
            Arc::new(TestLogger {}),
        )
        .await;
        let mutiny_ws = MutinySocketDescriptor::Mutiny(sub_ws_socket);

        let (send_to_multi_socket_2, _): (Sender<Message>, Receiver<Message>) = unbounded();

        let socket_map_2: SubSocketMap = Arc::new(Mutex::new(HashMap::new()));
        let (send_to_sub_socket_2, read_from_multi_socket_2): (Sender<Message>, Receiver<Message>) =
            unbounded();
        let sub_ws_socket_2 = create_new_subsocket(
            socket_map_2.clone(),
            send_to_multi_socket_2.clone(),
            send_to_sub_socket_2,
            read_from_multi_socket_2,
            PublicKey::from_str(OTHER_PEER_PUBKEY).unwrap().encode(),
            PublicKey::from_str(PEER_PUBKEY).unwrap().encode(),
            Arc::new(TestLogger {}),
        )
        .await;
        let mutiny_ws_2 = MutinySocketDescriptor::Mutiny(sub_ws_socket_2);
        assert_ne!(mutiny_ws, mutiny_ws_2);

        let (send_to_multi_socket_3, _): (Sender<Message>, Receiver<Message>) = unbounded();

        let socket_map_3: SubSocketMap = Arc::new(Mutex::new(HashMap::new()));
        let (send_to_sub_socket_3, read_from_multi_socket_3): (Sender<Message>, Receiver<Message>) =
            unbounded();
        let sub_ws_socket_3 = create_new_subsocket(
            socket_map_3.clone(),
            send_to_multi_socket_3.clone(),
            send_to_sub_socket_3,
            read_from_multi_socket_3,
            PublicKey::from_str(OTHER_PEER_PUBKEY).unwrap().encode(),
            PublicKey::from_str(PEER_PUBKEY).unwrap().encode(),
            Arc::new(TestLogger {}),
        )
        .await;
        let mutiny_ws_3 = MutinySocketDescriptor::Mutiny(sub_ws_socket_3);
        assert_eq!(mutiny_ws_3.clone(), mutiny_ws_3);
    }
}
