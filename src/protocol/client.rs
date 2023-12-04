use std::{io::Cursor, net::SocketAddr};

use crate::{buffer_pool::BufPool, prelude::PacketeerError, Packeteer};

use super::*;

#[derive(Debug, PartialEq)]
pub enum ClientState {
    SendingConnectionRequest,
    SendingChallengeResponse,
    Connected,
    Disconnected(DisconnectReason),
    SelfInitiatedDisconnect,
}

#[derive(Debug, PartialEq)]
pub enum DisconnectReason {
    Normal,
    TimedOut,
    // SelfInitiated,
    ServerDenied(u8),
}

// TODO need to move lib to being an endpoint that doesn't decode the messages body.. just headers seq and tracking?
// then the top level thing contains the protocol stuff plus the endpoint, plus the msg decoding thing

/// This can get wrapped up in a bevy plugin with a transport, and packets can be shuttled
/// to and fro with the transport in the plugin.
pub struct ProtocolClient {
    pub(crate) time: f64,
    state: ClientState,
    client_salt: Option<u64>,
    server_salt: Option<u64>,
    xor_salt: Option<u64>,
    proto_last_send: f64,
    out_buffer: Vec<AddressedPacket>,
    last_receive_time: f64,
    last_send_time: f64,
    pub(crate) packeteer: Packeteer,
    server_addr: SocketAddr,
    pool: BufPool,
}

impl ProtocolClient {
    pub fn new(time: f64) -> Self {
        Self {
            time,
            state: ClientState::Disconnected(DisconnectReason::Normal),
            client_salt: None,
            server_salt: None,
            xor_salt: None,
            proto_last_send: 0.0,
            last_receive_time: 0.0,
            last_send_time: 0.0,
            out_buffer: Vec::new(),
            packeteer: Packeteer::default(),
            server_addr: "127.0.0.1:6000".parse().expect("Invalid server address"),
            pool: BufPool::default(),
        }
    }

    pub fn update(&mut self, dt: f64) {
        self.time += dt;
        if !matches!(self.state, ClientState::Disconnected(_))
            && self.time - self.last_receive_time > 5.
        {
            self.state_transition(ClientState::Disconnected(DisconnectReason::TimedOut));
        }
    }

    fn state_transition(&mut self, new_state: ClientState) {
        log::info!("Client transition {:?} --> {new_state:?}", self.state);
        self.state = new_state;
        self.proto_last_send = 0.0;

        self.packeteer.set_xor_salt(self.xor_salt);
    }

    /// Cleanly disconnect from the server. This may add packets to the out_buffer for you
    /// to send, remember to do the actual send..
    pub fn disconnect(&mut self) {
        match self.state {
            ClientState::Connected | ClientState::SendingChallengeResponse => {
                // when disconnecting, we discard anything waiting to be sent,
                // because we'll be writing a load of disconnect packets in there.
                self.out_buffer.clear();
                self.state_transition(ClientState::SelfInitiatedDisconnect);
            }
            _ => {
                // disconnecting in this state doesn't require sending packets, so we just
                // transition directly to Disconnected.
                self.out_buffer.clear();
                self.state_transition(ClientState::Disconnected(DisconnectReason::Normal));
            }
        }
    }

    pub fn connect(&mut self) {
        self.server_salt = None;
        self.xor_salt = None;
        self.client_salt = Some(rand::random::<u64>());
        log::info!(
            "Client::connect - assigned ourselves salt: {:?}",
            self.client_salt
        );
        self.state_transition(ClientState::SendingConnectionRequest);
    }

    /// Serializes the ProtocolPacket, wraps in AddressedPacket with the server_addr, and appends
    /// to self.out_buffer for sending.
    pub(crate) fn send(&mut self, packet: ProtocolPacket) -> Result<(), PacketeerError> {
        let ap = AddressedPacket {
            address: self.server_addr,
            packet: write_packet(&self.pool, packet)?,
        };
        self.out_buffer.push(ap);
        self.last_send_time = self.time;
        Ok(())
    }

    /// In case we aren't fully connected, this could be resending challenge responses etc.
    /// If we are connected, it delegates to packeteer for messages packets.
    pub fn drain_packets_to_send(
        &mut self,
    ) -> Result<std::vec::Drain<'_, AddressedPacket>, PacketeerError> {
        match self.state {
            ClientState::SelfInitiatedDisconnect if self.xor_salt.is_some() => {
                for _ in 0..10 {
                    let d = ProtocolPacket::Disconnect(DisconnectPacket {
                        header: self.packeteer.next_packet_header(PacketType::Disconnect)?,
                        xor_salt: self.xor_salt.unwrap(),
                    });
                    self.send(d)?;
                }
                log::info!("Reseting Protocol Client state after disconnect");
                // TODO reset more, like packeteer
                self.client_salt = None;
                self.server_salt = None;
                self.xor_salt = None;
            }

            ClientState::SendingConnectionRequest if self.proto_last_send < self.time - 0.1 => {
                self.proto_last_send = self.time;
                let header = self
                    .packeteer
                    .next_packet_header(PacketType::ConnectionRequest)?;
                self.send(ProtocolPacket::ConnectionRequest(ConnectionRequestPacket {
                    protocol_version: PROTOCOL_VERSION,
                    client_salt: self.client_salt.unwrap(),
                    header,
                }))?;
            }

            ClientState::SendingChallengeResponse
                if self.proto_last_send < self.time - 0.1 && self.xor_salt.is_some() =>
            {
                self.proto_last_send = self.time;
                let header = self
                    .packeteer
                    .next_packet_header(PacketType::ConnectionChallengeResponse)?;
                self.send(ProtocolPacket::ConnectionChallengeResponse(
                    ConnectionChallengeResponsePacket {
                        header,
                        xor_salt: self.xor_salt.unwrap(),
                    },
                ))?;
            }

            ClientState::Connected => {
                // TODO this would be better bypassing out_buffer
                self.out_buffer
                    .extend(
                        self.packeteer
                            .drain_packets_to_send()
                            .map(|buf| AddressedPacket {
                                address: self.server_addr,
                                packet: buf,
                            }),
                    );
                // send keepalive if nothing sent in a while
                if self.out_buffer.is_empty() && self.time - self.last_send_time >= 0.1 {
                    log::info!("Client is sending keepalive");
                    let keepalive = ProtocolPacket::KeepAlive(KeepAlivePacket {
                        header: self.packeteer.next_packet_header(PacketType::KeepAlive)?,
                        client_index: 0,
                        xor_salt: self
                            .xor_salt
                            .expect("Should be an xor_salt in Connected state"),
                    });
                    self.send(keepalive)?;
                }
            }

            _ => {}
        }
        Ok(self.out_buffer.drain(..))
    }

    pub fn receive(&mut self, packet: &[u8], source: SocketAddr) -> Result<(), PacketeerError> {
        assert_eq!(source, self.server_addr, "source and server addr mismatch"); // TODO
        let packet_len = packet.len();
        let mut cur = Cursor::new(packet);
        let new_state = {
            let packet = read_packet(&mut cur, &self.pool)?;
            self.last_receive_time = self.time;
            log::info!("client got >>> {packet:?}");
            match packet {
                ProtocolPacket::ConnectionChallenge(ConnectionChallengePacket {
                    header: _,
                    client_salt,
                    server_salt,
                }) if self.state == ClientState::SendingConnectionRequest => {
                    if let Some(self_client_salt) = self.client_salt {
                        if self_client_salt != client_salt {
                            log::warn!("client salt mismatch: {packet:?}");
                            return Err(PacketeerError::InvalidPacket); // TODO error
                        }
                        self.server_salt = Some(server_salt);
                        let client_salt_xor_server_salt = server_salt ^ self_client_salt;
                        self.xor_salt = Some(client_salt_xor_server_salt);
                        Some(ClientState::SendingChallengeResponse)
                    } else {
                        None
                    }
                }
                ProtocolPacket::ConnectionDenied(ConnectionDeniedPacket { header: _, reason }) => {
                    if self.state == ClientState::SendingConnectionRequest
                        || self.state == ClientState::SendingChallengeResponse
                    {
                        log::warn!("Connection was denied: {reason}");
                        self.server_salt = None;
                        self.xor_salt = None;
                        Some(ClientState::Disconnected(DisconnectReason::ServerDenied(
                            reason,
                        )))
                    } else {
                        None
                    }
                }
                ProtocolPacket::KeepAlive(KeepAlivePacket {
                    header,
                    xor_salt,
                    client_index: _,
                }) if matches!(
                    self.state,
                    ClientState::Connected | ClientState::SendingChallengeResponse
                ) =>
                {
                    if self.xor_salt == Some(xor_salt) {
                        self.last_receive_time = self.time;
                        if self.state == ClientState::SendingChallengeResponse {
                            Some(ClientState::Connected)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                ProtocolPacket::Messages(mp) if self.state == ClientState::Connected => {
                    if self.xor_salt == Some(mp.xor_salt) {
                        self.packeteer.process_incoming_packet(packet_len, mp)?;
                    }
                    None
                }
                p => {
                    log::warn!(
                        "Discarding unhandled {p:?} in client state: {:?}",
                        self.state
                    );
                    None
                }
            }
        };
        if let Some(new_state) = new_state {
            self.state_transition(new_state);
        }
        Ok(())
    }
}
