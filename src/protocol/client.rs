use std::{io::Cursor, net::SocketAddr};

use crate::{buffer_pool::BufHandle, prelude::PicklebackError, Pickleback};

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

pub struct PicklebackClient {
    pub(crate) time: f64,
    state: ClientState,
    client_salt: Option<u64>,
    server_salt: Option<u64>,
    xor_salt: Option<u64>,
    proto_last_send: f64,
    last_receive_time: f64,
    last_send_time: f64,
    pub(crate) pickleback: Pickleback,
    server_addr: SocketAddr,
}

impl PicklebackClient {
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
            pickleback: Pickleback::default(),
            server_addr: "127.0.0.1:6000".parse().expect("Invalid server address"),
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
        self.pickleback.set_xor_salt(self.xor_salt);
        if matches!(self.state, ClientState::Disconnected(_)) {
            self.reset();
        }
    }

    fn reset(&mut self) {
        log::info!("Reseting ProtocolClient");
        self.client_salt = None;
        self.server_salt = None;
        self.xor_salt = None;
        self.proto_last_send = 0.0;
        self.last_receive_time = 0.0;
        self.last_send_time = 0.0;
        self.pickleback = Pickleback::default();
    }

    /// Cleanly disconnect from the server. This may add packets to the out_buffer for you
    /// to send, remember to do the actual send..
    pub fn disconnect(&mut self) {
        match self.state {
            ClientState::Connected | ClientState::SendingChallengeResponse => {
                // when disconnecting, we discard anything waiting to be sent,
                // because we'll be writing a load of disconnect packets in there.
                self.state_transition(ClientState::SelfInitiatedDisconnect);
            }
            _ => {
                // disconnecting in this state doesn't require sending packets, so we just
                // transition directly to Disconnected.
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
    pub(crate) fn send(&mut self, packet: ProtocolPacket) -> Result<(), PicklebackError> {
        self.pickleback.send_packet(packet)?;
        self.last_send_time = self.time;
        Ok(())
    }

    /// Calls send_fn with every packet ready to send.
    ///
    /// In case we aren't fully connected, this could be resending challenge responses etc.
    /// If we are connected, it delegates to pickleback for messages packets.
    pub fn visit_packets_to_send(
        &mut self,
        mut send_fn: impl FnMut(SocketAddr, BufHandle),
    ) -> Result<(), PicklebackError> {
        match self.state {
            ClientState::SelfInitiatedDisconnect if self.xor_salt.is_some() => {
                for _ in 0..10 {
                    let d = ProtocolPacket::Disconnect(DisconnectPacket {
                        header: self.pickleback.next_packet_header(PacketType::Disconnect)?,
                        xor_salt: self.xor_salt.unwrap(),
                    });
                    self.send(d)?;
                }
                log::info!("Reseting Protocol Client state after disconnect");
                self.state_transition(ClientState::Disconnected(DisconnectReason::Normal));
            }

            ClientState::SendingConnectionRequest if self.proto_last_send < self.time - 0.1 => {
                self.proto_last_send = self.time;
                let header = self
                    .pickleback
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
                    .pickleback
                    .next_packet_header(PacketType::ConnectionChallengeResponse)?;
                self.send(ProtocolPacket::ConnectionChallengeResponse(
                    ConnectionChallengeResponsePacket {
                        header,
                        xor_salt: self.xor_salt.unwrap(),
                    },
                ))?;
            }

            ClientState::Connected => {
                // should happen in update?
                let backpressure = match self.pickleback.compose_packets() {
                    Ok(_) => false,
                    Err(PicklebackError::Backpressure(_)) => true,
                    Err(e) => return Err(e),
                };
                // send keepalive if nothing sent in a while (unless backpressure)
                if !backpressure
                    && !self.pickleback.has_packets_to_send()
                    && self.time - self.last_send_time >= 0.1
                {
                    log::info!("Client is sending keepalive");
                    let keepalive = ProtocolPacket::KeepAlive(KeepAlivePacket {
                        header: self.pickleback.next_packet_header(PacketType::KeepAlive)?,
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
        self.pickleback
            .drain_packets_to_send()
            .for_each(|packet| send_fn(self.server_addr, packet));
        Ok(())
    }

    pub fn receive(&mut self, packet: &[u8], source: SocketAddr) -> Result<(), PicklebackError> {
        assert_eq!(source, self.server_addr, "source and server addr mismatch"); // TODO
        let mut cur = Cursor::new(packet);
        let new_state = {
            let packet = read_packet(&mut cur)?;
            self.last_receive_time = self.time;
            log::info!("client got >>> {packet:?}");
            match packet {
                ProtocolPacket::ConnectionChallenge(ConnectionChallengePacket {
                    header,
                    client_salt,
                    server_salt,
                }) if self.state == ClientState::SendingConnectionRequest => {
                    if let Some(self_client_salt) = self.client_salt {
                        if self_client_salt != client_salt {
                            log::warn!("client salt mismatch");
                            return Err(PicklebackError::InvalidPacket); // TODO error
                        }
                        self.server_salt = Some(server_salt);
                        let client_salt_xor_server_salt = server_salt ^ self_client_salt;
                        self.xor_salt = Some(client_salt_xor_server_salt);
                        self.pickleback.process_incoming_packet(&header, &mut cur)?;
                        Some(ClientState::SendingChallengeResponse)
                    } else {
                        None
                    }
                }
                ProtocolPacket::ConnectionDenied(ConnectionDeniedPacket { header, reason }) => {
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
                        self.pickleback.process_incoming_packet(&header, &mut cur)?;
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
                        // this will consume the remaining cursor and parse out the messages
                        self.pickleback
                            .process_incoming_packet(&mp.header, &mut cur)?;
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
