use super::*;
use crate::{
    buffer_pool::BufPool,
    prelude::{PacketeerConfig, PacketeerError},
    PacketId, Packeteer,
};
use std::{collections::VecDeque, io::Cursor, iter::empty, net::SocketAddr};

// when server gets a ConnectionRequest, it sends a Connection Challenge Packet and creates
// a PendingClient.
pub(crate) struct PendingClient {
    client_salt: u64,
    server_salt: u64,
    first_requested_time: f64,
    // time we first sent challenge response
    last_challenged_at: f64,
    socket_addr: SocketAddr,
    packeteer: Packeteer,
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

// When server gets a Connection Challenge Response, it promotes from PendingClient to ConnectedClient
pub(crate) struct ConnectedClient {
    client_salt: u64,
    server_salt: u64,
    xor_salt: u64,
    socket_addr: SocketAddr,
    /// time we last received a packet from client
    last_received_time: f64,
    /// time we last sent a packet to this client
    last_sent_time: f64,
    /// endpoint for messages
    packeteer: Packeteer,
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

// this should hold the packeteer instance?
pub(crate) struct ProtocolServer<'a> {
    time: f64,
    pending_clients: Vec<PendingClient>,
    connected_clients: Vec<ConnectedClient>,
    outbox: VecDeque<AddressedPacket>,
    pool: &'a BufPool,
}

impl<'a> ProtocolServer<'a> {
    fn new(time: f64, pool: &'a BufPool) -> Self {
        Self {
            time,
            pending_clients: Vec::new(),
            connected_clients: Vec::new(),
            outbox: VecDeque::new(),
            pool,
        }
    }
    pub(crate) fn update(&mut self, dt: f64) -> Result<(), PacketeerError> {
        self.time += dt;
        self.compose_packets()?;
        Ok(())
    }

    pub fn drain_packets_to_send(
        &mut self,
    ) -> std::collections::vec_deque::Drain<'_, AddressedPacket> {
        self.outbox.drain(..)
    }

    fn compose_packets(&mut self) -> Result<(), PacketeerError> {
        let mut to_remove = Vec::new();

        // Run state-machine for pending clients:
        // TODO need to time out and clean up quite pending clients
        for i in 0..self.pending_clients.len() {
            let pc = &mut self.pending_clients[i];
            if self.time - pc.first_requested_time > 5000. {
                to_remove.push(i);
                continue;
            }
            if pc.last_challenged_at < self.time - 0.1 {
                pc.last_challenged_at = self.time;
                let packet = ProtocolPacket::ConnectionChallenge(ConnectionChallengePacket {
                    header: pc
                        .packeteer
                        .next_packet_header(PacketType::ConnectionChallenge)?,
                    client_salt: pc.client_salt,
                    server_salt: pc.server_salt,
                });

                let packet = write_packet(self.pool, packet)?;
                self.outbox.push_back(AddressedPacket {
                    address: pc.socket_addr,
                    packet,
                });
            }
        }
        for i in to_remove.drain(..) {
            let pc = self.pending_clients.remove(i);
            log::info!("Removed timed-out pending client: {pc:?}");
        }
        // for connected clients, we must timeout any that are awol
        // extract_if (drain_filter for vec) isn't stable yet..
        for i in 0..self.connected_clients.len() {
            let cc = &self.connected_clients[i];
            if self.time - cc.last_received_time > 5000. {
                to_remove.push(i);
            }
        }
        for i in to_remove {
            let cc = self.connected_clients.remove(i);
            log::info!("Timed out client: {cc:?}");
        }
        // for connected clients, we send any messages that the packeteer layer wants
        for cc in self.connected_clients.iter_mut() {
            self.outbox.extend(
                cc.packeteer
                    .drain_packets_to_send()
                    .map(|packet| AddressedPacket {
                        address: cc.socket_addr,
                        packet,
                    }),
            )
        }
        Ok(())
    }

    fn get_pending_by_client_salt(
        &mut self,
        client_salt: u64,
        client_addr: SocketAddr,
    ) -> Option<&PendingClient> {
        for pc in self.pending_clients.iter_mut() {
            if pc.client_salt == client_salt && pc.socket_addr == client_addr {
                return Some(pc);
            }
        }
        None
    }
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
    fn get_connected_client_mut(
        &mut self,
        xor_salt: u64,
        client_addr: SocketAddr,
    ) -> Option<&mut ConnectedClient> {
        for cc in self.connected_clients.iter_mut() {
            if cc.xor_salt == xor_salt && cc.socket_addr == client_addr {
                return Some(cc);
            }
        }
        None
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
            return Some(self.connected_clients.remove(index));
        }
        None
    }

    pub fn receive(
        &mut self,
        packet: &[u8],
        client_addr: SocketAddr,
    ) -> Result<(), PacketeerError> {
        let packet_len = packet.len();
        let mut cur = Cursor::new(packet);
        let packet = read_packet(&mut cur, self.pool)?;
        match packet {
            ProtocolPacket::ConnectionRequest(ConnectionRequestPacket {
                header: _,
                client_salt,
                protocol_version,
            }) => {
                if protocol_version != PROTOCOL_VERSION {
                    log::warn!("Protocol version mismatch");
                    let denied = write_packet(
                        self.pool,
                        ProtocolPacket::ConnectionDenied(ConnectionDeniedPacket {
                            header: ProtocolPacketHeader::new(
                                PacketId(0),
                                empty(),
                                0,
                                PacketType::ConnectionDenied,
                                None,
                            )?,
                            reason: 0, // TODO
                        }),
                    )?;
                    self.outbox.push_back(AddressedPacket {
                        address: client_addr,
                        packet: denied,
                    });
                    return Ok(());
                }
                if let Some(_pending) = self.get_pending_by_client_salt(client_salt, client_addr) {
                    // safe to ignore, already pending. will continue to receive challenge msgs.
                } else {
                    let server_salt = rand::random::<u64>();
                    self.pending_clients.push(PendingClient {
                        client_salt,
                        server_salt,
                        last_challenged_at: 0.0,
                        socket_addr: client_addr,
                        first_requested_time: self.time,
                        packeteer: Packeteer::new(PacketeerConfig::default(), self.time),
                    })
                }
            }
            ProtocolPacket::ConnectionChallengeResponse(ConnectionChallengeResponsePacket {
                header,
            }) if header.xor_salt.is_some() => {
                // if pending, set 'em up.
                // if already connected from our POV, they may not have got the message yet?
                //
                // we should probablt send a welcome packet, and always send a welcome when talking
                // to a CC if we have yet to receive a message from them while in the connected state?
                if let Some(pending) =
                    self.take_pending_by_xor_salt(header.xor_salt.unwrap(), client_addr)
                {
                    let cc = ConnectedClient {
                        client_salt: pending.client_salt,
                        server_salt: pending.server_salt,
                        xor_salt: pending.client_salt ^ pending.server_salt,
                        last_received_time: self.time,
                        last_sent_time: self.time,
                        socket_addr: client_addr,
                        packeteer: pending.packeteer,
                    };
                    log::info!("New CC");
                    self.connected_clients.push(cc);
                    // send welcome packet?
                } else {
                    let denied = write_packet(
                        self.pool,
                        ProtocolPacket::ConnectionDenied(ConnectionDeniedPacket {
                            header: ProtocolPacketHeader::new(
                                PacketId(0),
                                empty(),
                                0,
                                PacketType::ConnectionDenied,
                                None,
                            )?,
                            reason: 0,
                        }),
                    )?;
                    self.outbox.push_back(AddressedPacket {
                        address: client_addr,
                        packet: denied,
                    });
                }
            }

            ProtocolPacket::Disconnect(DisconnectPacket { header })
                if header.xor_salt.is_some() =>
            {
                if let Some(cc) =
                    self.remove_connected_client(header.xor_salt.unwrap(), client_addr)
                {
                    log::info!("REMOVED CLIENT: {:?}", cc.socket_addr);
                }
            }

            ProtocolPacket::Messages(mut mp) => {
                let time = self.time;
                if let Some(cc) =
                    self.get_connected_client_mut(mp.header.xor_salt.unwrap(), client_addr)
                {
                    cc.last_received_time = time;
                    cc.packeteer.process_incoming_packet(packet_len, mp)?;
                }
            }
            p => {
                log::warn!("Server Discarding unhandled {p:?}");
            }
        }
        Ok(())
    }
}
