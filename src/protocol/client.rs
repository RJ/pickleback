use std::{io::Cursor, net::SocketAddr};

use crate::{buffer_pool::BufPool, prelude::PacketeerError, Packeteer};

use super::*;

#[derive(Debug, PartialEq)]
pub(crate) enum ClientState {
    SendingConnectionRequest,
    SendingChallengeResponse,
    Connected,
    Disconnected,
    SelfInitiatedDisconnect,
}

// TODO need to move lib to being an endpoint that doesn't decode the messages body.. just headers seq and tracking?
// then the top level thing contains the protocol stuff plus the endpoint, plus the msg decoding thing

/// This can get wrapped up in a bevy plugin with a transport, and packets can be shuttled
/// to and fro with the transport in the plugin.
pub(crate) struct ProtocolClient<'a> {
    time: f64,
    state: ClientState,
    client_salt: Option<u64>,
    server_salt: Option<u64>,
    xor_salt: Option<u64>,
    proto_last_send: f64,
    out_buffer: Vec<AddressedPacket>,
    pending_disconnect: bool,
    packeteer: Packeteer,
    server_addr: SocketAddr,
    pool: &'a BufPool,
}

impl<'a> ProtocolClient<'a> {
    pub fn new(time: f64, pool: &'a BufPool) -> Self {
        Self {
            time,
            state: ClientState::Disconnected,
            client_salt: None,
            server_salt: None,
            xor_salt: None,
            proto_last_send: 0.0,
            out_buffer: Vec::new(),
            pending_disconnect: false,
            packeteer: Packeteer::default(),
            server_addr: "127.0.0.1:6000".parse().expect("Invalid server address"),
            pool,
        }
    }
    pub fn update(&mut self, dt: f64) {
        self.time += dt;
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
                self.state_transition(ClientState::Disconnected);
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
            packet: write_packet(self.pool, packet)?,
        };
        self.out_buffer.push(ap);
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

            ClientState::SendingChallengeResponse if self.proto_last_send < self.time - 0.1 => {
                self.proto_last_send = self.time;
                let header = self
                    .packeteer
                    .next_packet_header(PacketType::ConnectionChallengeResponse)?;
                self.send(ProtocolPacket::ConnectionChallengeResponse(
                    ConnectionChallengeResponsePacket { header },
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
                    )
            }

            _ => {}
        }
        Ok(self.out_buffer.drain(..))
    }

    pub fn receive(&mut self, packet: &[u8], source: SocketAddr) -> Result<(), PacketeerError> {
        assert_eq!(source, self.server_addr, "source and server addr mismatch"); // TODO
        let packet_len = packet.len();
        let mut cur = Cursor::new(packet);
        let packet = read_packet(&mut cur, self.pool)?;
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
                    self.state_transition(ClientState::SendingChallengeResponse);
                }
            }
            ProtocolPacket::ConnectionDenied(ConnectionDeniedPacket { header: _, reason }) => {
                if self.state == ClientState::SendingConnectionRequest
                    || self.state == ClientState::SendingChallengeResponse
                {
                    log::warn!("Connection was denied: {reason}");
                    self.server_salt = None;
                    self.xor_salt = None;
                    self.state_transition(ClientState::Disconnected);
                }
            }
            ProtocolPacket::Messages(mut mp)
                if self.xor_salt == mp.header.xor_salt // Some(client_salt_xor_server_salt)
                    && (self.state == ClientState::Connected
                        || self.state == ClientState::SendingChallengeResponse) =>
            {
                if self.state == ClientState::SendingChallengeResponse {
                    // we must be connected now, since the xor_salt matches and we've sent the challenge response.
                    self.state_transition(ClientState::Connected);
                }
                self.packeteer.process_incoming_packet(packet_len, mp);
                // TODO messages iter to avoid vec?
            }
            p => {
                log::warn!(
                    "Discarding unhandled {p:?} in client state: {:?}",
                    self.state
                );
            }
        }
        Ok(())
    }
}
