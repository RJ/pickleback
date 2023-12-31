use crate::*;
use std::{collections::VecDeque, io::Cursor, iter::empty, net::SocketAddr};

// TODO newtype ClientId(u64) ?

/// Emitted when the server connects or disconnects a client
#[derive(Debug, Clone)]
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Event))]
pub enum ServerEvent {
    /// A new client connected
    ClientConnected(u64),
    /// A client disconnected
    ClientDisconnected(u64, DisconnectReason),
}

// Pending clients are created from a ConnectionRequest, and subject to promtion to a fully
// `ConnectedClient` if the handshake procedure is completed before a timeout.
pub(crate) struct PendingClient {
    client_salt: u64,
    server_salt: u64,
    first_requested_time: f64,
    // time we first sent challenge response
    last_challenged_at: f64,
    socket_addr: SocketAddr,
    pickleback: Pickleback,
}

impl std::fmt::Debug for PendingClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PendingClient[{:?}/{:?}]",
            self.socket_addr, self.client_salt
        )
    }
}

/// The server's representation of a fully connected client that has completed handshaking.
pub struct ConnectedClient {
    xor_salt: u64,
    socket_addr: SocketAddr,
    confirmed: bool,
    client_index: Option<u32>,
    /// time we last received a packet from client
    last_received_time: f64,
    /// time we last sent a packet to this client
    last_sent_time: f64,
    /// endpoint for messages
    pickleback: Pickleback,
}

impl std::fmt::Debug for ConnectedClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ConnectedClient[{:?}/{:?}]",
            self.socket_addr, self.xor_salt
        )
    }
}

impl ConnectedClient {
    /// Unique client session id
    pub fn id(&self) -> u64 {
        self.xor_salt
    }

    /// Gets SocketAddr of remote client
    pub fn socket_addr(&self) -> SocketAddr {
        self.socket_addr
    }

    /// Send a payload on a channel.
    ///
    /// This yields a MessageId, which you can use to check acks with `drain_acks()`.
    ///
    /// # Errors
    ///
    /// Can error on backpressure, nosuchchannel, payloadtoobig
    pub fn send_message(
        &mut self,
        channel: u8,
        message_payload: &[u8],
    ) -> Result<MessageId, PicklebackError> {
        self.pickleback.send_message(channel, message_payload)
    }

    /// Draining iterator over new message acks since last call
    pub fn drain_acks(&mut self, channel: u8) -> std::vec::Drain<'_, MessageId> {
        self.pickleback.drain_message_acks(channel)
    }

    /// Gets new messages received on `channel` since last call, returned in a
    /// `ReceivedMessagesContainer` which isn't quite an iterator.
    ///
    /// # Example:
    ///
    /// ```rust,ignore
    /// let mut msgs = cc.get_messages(channel);
    /// while let Some(msg) = msgs.next() {
    ///     handle_message(msg);
    /// }
    /// ````
    pub fn get_messages(&mut self, channel: u8) -> ReceivedMessagesContainer {
        self.pickleback.drain_received_messages(channel)
    }

    /// Returns `PicklebackStats`, which tracks metrics on packet and message counts, etc.
    pub fn stats(&self) -> &PicklebackStats {
        self.pickleback.stats()
    }

    /// Round-trip-time estimation in seconds.
    pub fn rtt(&self) -> f32 {
        self.pickleback.rtt()
    }

    /// Packet loss estimation percent. 1.0 = 100% loss.
    pub fn packet_loss(&self) -> f32 {
        self.pickleback.packet_loss()
    }
}

/// Server that manages connections to multiple clients.
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Resource))]
pub struct PicklebackServer {
    pub(crate) time: f64,
    pending_clients: Vec<PendingClient>,
    connected_clients: Vec<ConnectedClient>,
    tmp_buffer: Vec<usize>,
    config: PicklebackConfig,
    pool: BufPool,
    // for arbitrary responses before clients are connected, like denied packets:
    outbox: VecDeque<AddressedPacket>,
    server_events: VecDeque<ServerEvent>,
}

impl PicklebackServer {
    /// New PicklebackServer
    pub fn new(time: f64, config: &PicklebackConfig) -> Self {
        Self {
            time,
            pending_clients: Vec::new(),
            connected_clients: Vec::new(),
            tmp_buffer: Vec::new(),
            config: config.clone(),
            pool: BufPool::full_packets_only(),
            outbox: VecDeque::new(),
            server_events: VecDeque::new(),
        }
    }
    /// internal time value
    pub fn time(&self) -> f64 {
        self.time
    }
    /// Cleanly disconnects a client
    pub fn disconnect_client(&mut self, xor_salt: u64) {
        if let Some(cc) = self.get_connected_client_by_salt_mut(xor_salt) {
            if let Ok(header) = cc.pickleback.next_packet_header(PacketType::Disconnect) {
                let dis = DisconnectPacket { header, xor_salt };
                let d = ProtocolPacket::Disconnect(dis);
                // disconnecting, so ignore failures.. TODO spam multiple
                // should maybe clear anything in the outbox at this point too before sending discos
                let _ = cc.pickleback.send_packet(d);
            }
        } else {
            return;
        }
        self.connected_clients.retain(|cc| cc.xor_salt != xor_salt);
    }

    /// Send a message to a client by client id.
    ///
    /// Sugar, since you can also call send_message on the `ConnectedClient`
    ///
    /// # Errors
    /// can throw NoSuchClient, amongst others
    pub fn send_message(
        &mut self,
        client_id: u64,
        channel: u8,
        message_payload: &[u8],
    ) -> Result<MessageId, PicklebackError> {
        let Some(cc) = self.get_connected_client_by_salt_mut(client_id) else {
            return Err(PicklebackError::NoSuchClient);
        };
        cc.send_message(channel, message_payload)
    }

    /// Broadcast a message to all connected clients, discarding errors and MessageIds
    pub fn broadcast_message(&mut self, channel: u8, message_payload: &[u8]) {
        for cc in self.connected_clients_mut() {
            let _ = cc.send_message(channel, message_payload);
        }
    }

    /// All clients in the Connected state
    pub fn connected_clients_mut(&mut self) -> impl Iterator<Item = &mut ConnectedClient> {
        self.connected_clients.iter_mut()
    }

    /// Advances time, and the client connection state machines.
    pub fn update(&mut self, dt: f64) {
        self.time += dt;
        self.process_pending_clients(dt);
        self.process_connected_clients(dt);
    }

    /// Send all pending outbound packets for all clients
    pub fn visit_packets_to_send(&mut self, mut send_fn: impl FnMut(SocketAddr, &[u8])) {
        // the packets the server generates that don't belong to a (pending)client's pickleback:
        for AddressedPacket { address, packet } in self.outbox.drain(..) {
            send_fn(address, packet.as_ref());
        }
        for pc in &mut self.pending_clients {
            let _ = pc
                .pickleback
                .visit_packets_to_send(|packet| send_fn(pc.socket_addr, packet));
        }
        for cc in &mut self.connected_clients {
            let num_sent = cc
                .pickleback
                .visit_packets_to_send(|packet| send_fn(cc.socket_addr, packet));
            if num_sent > 0 {
                cc.last_sent_time = self.time;
            }
        }
    }

    /// Draining iter over new server events for detecting client connects and disconnects
    pub fn drain_server_events(
        &mut self,
    ) -> std::collections::vec_deque::Drain<'_, server::ServerEvent> {
        self.server_events.drain(..)
    }

    /// Processes pending clients, catching and handling any errors.
    ///
    /// We don't want an error thrown by one client to abort processing for other clients.
    fn process_pending_clients(&mut self, dt: f64) {
        self.tmp_buffer.clear();
        for i in 0..self.pending_clients.len() {
            let pc = &mut self.pending_clients[i];
            pc.pickleback.update(dt);
            // client's can't be pending for more than 5 seconds
            if self.time - pc.first_requested_time > CONNECTION_TIMEOUT {
                self.tmp_buffer.push(i);
                continue;
            }
            // resend challenges every 100ms
            if pc.last_challenged_at < self.time - HANDSHAKE_RESEND_INTERVAL {
                pc.last_challenged_at = self.time;
                let header = match pc
                    .pickleback
                    .next_packet_header(PacketType::ConnectionChallenge)
                {
                    Ok(h) => h,
                    Err(PicklebackError::Backpressure(_)) => continue,
                    Err(e) => {
                        log::error!("Pending client err: {e:?}");
                        continue;
                    }
                };
                let packet = ProtocolPacket::ConnectionChallenge(ConnectionChallengePacket {
                    header,
                    client_salt: pc.client_salt,
                    server_salt: pc.server_salt,
                });
                match pc.pickleback.send_packet(packet) {
                    Ok(_) => {}
                    Err(PicklebackError::Backpressure(_)) => {}
                    Err(e) => {
                        log::error!("Pending client: {e:?}");
                    }
                }
            }
        }
        for i in self.tmp_buffer.drain(..) {
            let pc = self.pending_clients.remove(i);
            log::info!("Removed timed-out pending client: {pc:?}");
        }
    }

    /// Processes connected clients, catching and handling any errors.
    ///
    /// We don't want an error thrown by one client to abort processing for other clients.
    fn process_connected_clients(&mut self, dt: f64) {
        self.tmp_buffer.clear();
        // for connected clients, we must timeout any that are awol
        // extract_if (drain_filter for vec) isn't stable yet..
        for i in 0..self.connected_clients.len() {
            let cc = &self.connected_clients[i];
            if self.time - cc.last_received_time > CONNECTION_TIMEOUT {
                self.tmp_buffer.push(i);
            }
        }
        for i in self.tmp_buffer.drain(..) {
            let cc = self.connected_clients.remove(i);
            self.server_events
                .push_back(ServerEvent::ClientDisconnected(
                    cc.id(),
                    DisconnectReason::TimedOut,
                ));
            log::info!("Timed out client: {cc:?}");
        }
        // send any messages that the pickleback layer wants
        for cc in self.connected_clients.iter_mut() {
            cc.pickleback.update(dt);
            // * until confirmed, send a KA before messages packets
            // * if nothing sent for a while, send a KA
            let time_since_last_send = self.time - cc.last_sent_time;
            if (!cc.confirmed && cc.pickleback.num_packets_to_send() > 0)
                || time_since_last_send > KEEPALIVE_INTERVAL
            {
                warn!("Sending KA, ttls={time_since_last_send}");
                let header = match cc.pickleback.next_packet_header(PacketType::KeepAlive) {
                    Ok(h) => h,
                    Err(PicklebackError::Backpressure(_)) => continue,
                    Err(e) => {
                        log::error!("Pending client err: {e:?}");
                        continue;
                    }
                };

                let keepalive = ProtocolPacket::KeepAlive(KeepAlivePacket {
                    header,
                    xor_salt: cc.xor_salt,
                    client_index: cc.client_index.unwrap_or_default(), // TODO
                });
                match cc.pickleback.send_packet(keepalive) {
                    Ok(_) => {}
                    Err(PicklebackError::Backpressure(_)) => {}
                    Err(e) => {
                        log::error!("Connected client: {e:?}");
                    }
                }
            }
        }
    }

    fn get_pending_by_client_salt_mut(
        &mut self,
        client_salt: u64,
        client_addr: SocketAddr,
    ) -> Option<&mut PendingClient> {
        self.pending_clients
            .iter_mut()
            .find(|pc| pc.client_salt == client_salt && pc.socket_addr == client_addr)
    }
    // TODO this should be take by client salt/server salt? dont want people to be able to take using
    // sniffed salts
    fn take_pending_by_xor_salt(
        &mut self,
        xor_salt: u64,
        client_addr: SocketAddr,
    ) -> Option<PendingClient> {
        let mut i = None;
        for search in 0..self.pending_clients.len() {
            let pc = &mut self.pending_clients[search];
            if (pc.client_salt ^ pc.server_salt) == xor_salt && pc.socket_addr == client_addr {
                i = Some(search);
                break;
            }
        }
        if let Some(index) = i {
            return Some(self.pending_clients.remove(index));
        }
        None
    }

    /// Get a connected client by the xor salt id
    pub fn get_connected_client_by_salt_mut(
        &mut self,
        xor_salt: u64,
    ) -> Option<&mut ConnectedClient> {
        self.connected_clients
            .iter_mut()
            .find(|cc| cc.xor_salt == xor_salt)
    }
    fn get_connected_client_mut(
        &mut self,
        xor_salt: u64,
        client_addr: SocketAddr,
    ) -> Option<&mut ConnectedClient> {
        self.connected_clients
            .iter_mut()
            .find(|cc| cc.xor_salt == xor_salt && cc.socket_addr == client_addr)
    }

    fn remove_connected_client(
        &mut self,
        xor_salt: u64,
        client_addr: SocketAddr,
    ) -> Option<ConnectedClient> {
        let mut i = None;
        for search in 0..self.connected_clients.len() {
            let cc = &self.connected_clients[search];
            if cc.xor_salt == xor_salt && cc.socket_addr == client_addr {
                i = Some(search);
                break;
            }
        }
        if let Some(index) = i {
            let cc = self.connected_clients.remove(index);
            self.server_events
                .push_back(ServerEvent::ClientDisconnected(
                    cc.id(),
                    DisconnectReason::Normal,
                ));
            return Some(cc);
        }
        None
    }

    /// Process a packet received from the network.
    ///
    /// # Errors
    ///
    /// * Invalid packets cause an error
    /// * Errors sending to clients can cause error.
    ///
    /// However, this processes one packet at a time, so thrown errors won't prevent processing
    /// packets for other clients.
    pub fn receive(
        &mut self,
        packet: &[u8],
        client_addr: SocketAddr,
    ) -> Result<(), PicklebackError> {
        let mut cur = Cursor::new(packet);
        let packet = read_packet(&mut cur)?;
        log::info!("server got >>> {packet:?}");
        match packet {
            ProtocolPacket::ConnectionRequest(ConnectionRequestPacket {
                header,
                client_salt,
                protocol_version,
            }) => {
                if protocol_version != PROTOCOL_VERSION {
                    log::warn!("Protocol version mismatch");
                    let denied = write_packet(
                        &mut self.pool,
                        &self.config,
                        ProtocolPacket::ConnectionDenied(ConnectionDeniedPacket {
                            header: ProtocolPacketHeader::new(
                                PacketId(0),
                                empty(),
                                0,
                                PacketType::ConnectionDenied,
                            )?,
                            reason: DisconnectReason::ProtocolMismatch,
                        }),
                    )?;
                    self.outbox.push_back(AddressedPacket {
                        address: client_addr,
                        packet: (*denied).clone(),
                    });
                    self.pool.return_buffer(denied);
                    return Ok(());
                }
                if let Some(pc) = self.get_pending_by_client_salt_mut(client_salt, client_addr) {
                    // safe to ignore, already pending. will continue to receive challenge msgs.
                    return pc.pickleback.process_incoming_packet(&header, &mut cur);
                } else {
                    let server_salt = rand::random::<u64>();
                    let mut pc = PendingClient {
                        client_salt,
                        server_salt,
                        last_challenged_at: 0.0,
                        socket_addr: client_addr,
                        first_requested_time: self.time,
                        pickleback: Pickleback::new(PicklebackConfig::default(), self.time),
                    };
                    pc.pickleback.process_incoming_packet(&header, &mut cur)?;
                    self.pending_clients.push(pc);
                }
            }
            ProtocolPacket::ConnectionChallengeResponse(ConnectionChallengeResponsePacket {
                header,
                xor_salt,
            }) => {
                // there should be a pending client when we get a challenge response
                if let Some(pending) = self.take_pending_by_xor_salt(xor_salt, client_addr) {
                    if xor_salt != pending.client_salt ^ pending.server_salt {
                        // TODO return a denied packet?
                        return Err(PicklebackError::InvalidPacket);
                    }
                    let mut pickleback = pending.pickleback;
                    pickleback.set_xor_salt(Some(xor_salt));
                    pickleback.process_incoming_packet(&header, &mut cur)?;
                    let mut cc = ConnectedClient {
                        confirmed: false,
                        client_index: None,
                        xor_salt,
                        last_received_time: self.time,
                        last_sent_time: self.time,
                        socket_addr: client_addr,
                        pickleback,
                    };
                    // send KA - new connection, so not catching any errors here.
                    let ka = ProtocolPacket::KeepAlive(KeepAlivePacket {
                        header: cc.pickleback.next_packet_header(PacketType::KeepAlive)?,
                        xor_salt: cc.xor_salt,
                        client_index: 0,
                    });
                    cc.pickleback.send_packet(ka)?;
                    log::info!("New CC: {client_addr:?} = {cc:?}");
                    self.server_events
                        .push_back(ServerEvent::ClientConnected(cc.id()));
                    self.connected_clients.push(cc);
                } else {
                    if let Some(cc) = self.get_connected_client_by_salt_mut(xor_salt) {
                        // already established. dupe packet.
                        return cc.pickleback.process_incoming_packet(&header, &mut cur);
                    }
                    let denied = write_packet(
                        &mut self.pool,
                        &self.config,
                        ProtocolPacket::ConnectionDenied(ConnectionDeniedPacket {
                            header: ProtocolPacketHeader::new(
                                PacketId(0),
                                empty(),
                                0,
                                PacketType::ConnectionDenied,
                            )?,
                            reason: DisconnectReason::HandshakeTimeout,
                        }),
                    )?;
                    self.outbox.push_back(AddressedPacket {
                        address: client_addr,
                        packet: (*denied).clone(),
                    });
                    self.pool.return_buffer(denied);
                }
            }

            ProtocolPacket::Disconnect(DisconnectPacket {
                header: _,
                xor_salt,
            }) => {
                if let Some(cc) = self.remove_connected_client(xor_salt, client_addr) {
                    log::info!("REMOVED CLIENT: {:?}", cc.socket_addr);
                }
            }
            ProtocolPacket::KeepAlive(KeepAlivePacket {
                header,
                xor_salt,
                client_index: _, // reported by client. check it matches?
            }) => {
                let time = self.time;
                if let Some(cc) = self.get_connected_client_mut(xor_salt, client_addr) {
                    if !cc.confirmed {
                        log::info!("Marking cc as confirmed due to KA");
                        cc.confirmed = true;
                    }
                    cc.last_received_time = time;
                    cc.pickleback.process_incoming_packet(&header, &mut cur)?;
                } else {
                    log::warn!("Discarding KA packet, no session");
                }
            }
            ProtocolPacket::Messages(mp) => {
                let time = self.time;
                if let Some(cc) = self.get_connected_client_mut(mp.xor_salt, client_addr) {
                    cc.last_received_time = time;
                    if !cc.confirmed {
                        log::info!("Marking cc as confirmed due to messages");
                        cc.confirmed = true;
                    }
                    // this will consume the remaining cursor and parse out the messages
                    cc.pickleback
                        .process_incoming_packet(&mp.header, &mut cur)?;
                } else {
                    log::warn!(
                        "Server Discarding messages packet, no session {client_addr} = {mp:?}"
                    );
                    log::error!("connected_clients = {:?}", self.connected_clients);
                }
            }
            p => {
                log::error!("Server Discarding unhandled {p:?}");
            }
        }
        Ok(())
    }
}
