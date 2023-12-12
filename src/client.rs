use std::{io::Cursor, net::SocketAddr};

use crate::{
    // buffer_pool::BufHandle,
    prelude::{PicklebackConfig, PicklebackError},
    Pickleback,
};

use super::*;

/// Connection state of a client.
#[derive(Debug, PartialEq, Clone)]
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Event))]
pub enum ClientState {
    /// Initial connection establishment request
    SendingConnectionRequest,
    /// Responding to server's handshake challenge
    SendingChallengeResponse,
    /// Fully connected
    Connected,
    /// Disconnected with a reason
    Disconnected(DisconnectReason),
    /// In the midst of disconnecting cleanly, will soon be Disconnected.
    SelfInitiatedDisconnect,
}

/// The client end of a connection.
///
/// Connects to a `PicklebackServer` instance.
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Resource))]
pub struct PicklebackClient {
    time: f64,
    state: ClientState,
    client_salt: Option<u64>,
    server_salt: Option<u64>,
    xor_salt: Option<u64>,
    proto_last_send: f64,
    last_receive_time: f64,
    last_send_time: f64,
    pickleback: Pickleback,
    server_addr: Option<SocketAddr>,
    state_transitions: VecDeque<ClientState>,
}

impl PicklebackClient {
    /// Create a client, initially in a normal Disconnected state.
    pub fn new(time: f64, config: &PicklebackConfig) -> Self {
        Self {
            time,
            state: ClientState::Disconnected(DisconnectReason::Normal),
            client_salt: None,
            server_salt: None,
            xor_salt: None,
            proto_last_send: 0.0,
            last_receive_time: 0.0,
            last_send_time: 0.0,
            pickleback: Pickleback::new(config.clone(), 0.0),
            server_addr: None, //"127.0.0.1:6000".parse().expect("Invalid server address"),
            state_transitions: VecDeque::new(),
        }
    }

    /// Advance time. This can potentially result in this connection timing out.
    pub fn update(&mut self, dt: f64) {
        self.time += dt;
        self.pickleback.update(dt);
        if !matches!(self.state, ClientState::Disconnected(_))
            && self.time - self.last_receive_time > CONNECTION_TIMEOUT
        {
            self.state_transition(ClientState::Disconnected(DisconnectReason::TimedOut));
        }
    }

    /// Returns `PicklebackStats`, which tracks metrics on packet and message counts, etc.
    pub fn stats(&self) -> &PicklebackStats {
        self.pickleback.stats()
    }

    /// Round-trip-time estimation in seconds.
    pub fn rtt(&self) -> f32 {
        self.pickleback.rtt()
    }

    /// Packet loss estimation percent.
    pub fn packet_loss(&self) -> f32 {
        self.pickleback.packet_loss()
    }

    /// Draining iterator over new message acks for a given channel
    pub fn drain_message_acks(&mut self, channel: u8) -> std::vec::Drain<'_, MessageId> {
        self.pickleback.drain_message_acks(channel)
    }

    /// Gives an iterator of new messages received by this client
    pub fn drain_received_messages(&mut self, channel: u8) -> ReceivedMessagesContainer {
        self.pickleback.drain_received_messages(channel)
    }

    /// Drain state transitions since last check
    pub fn drain_state_transitions(
        &mut self,
    ) -> std::collections::vec_deque::Drain<'_, client::ClientState> {
        self.state_transitions.drain(..)
    }

    fn state_transition(&mut self, new_state: ClientState) {
        log::info!("Client transition {:?} --> {new_state:?}", self.state);
        self.state = new_state.clone();
        self.proto_last_send = 0.0;
        self.pickleback.set_xor_salt(self.xor_salt);
        if matches!(self.state, ClientState::Disconnected(_)) {
            self.reset();
        }
        self.state_transitions.push_back(new_state);
    }

    fn reset(&mut self) {
        log::debug!("Reseting PicklebackClient");
        self.client_salt = None;
        self.server_salt = None;
        self.xor_salt = None;
        self.proto_last_send = 0.0;
        self.last_receive_time = 0.0;
        self.last_send_time = 0.0;
        self.server_addr = None;
        let config = self.pickleback.config().clone();
        self.pickleback = Pickleback::new(config, 0.0);
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

    /// Returns ClientState
    pub fn state(&self) -> &ClientState {
        &self.state
    }

    /// Initiate connection to `server`.
    ///
    /// # Panics
    /// Will panic if server address can't parse to a SocketAddr.
    pub fn connect(&mut self, server_addr: SocketAddr) {
        self.server_addr = Some(server_addr);
        self.server_salt = None;
        self.xor_salt = None;
        self.client_salt = Some(rand::random::<u64>());
        log::debug!(
            "Client::connect - assigned ourselves salt: {:?}",
            self.client_salt
        );
        self.state_transition(ClientState::SendingConnectionRequest);
    }

    /// Serializes the ProtocolPacket and appends to self.out_buffer for sending.
    pub(crate) fn send(&mut self, packet: ProtocolPacket) -> Result<(), PicklebackError> {
        self.pickleback.send_packet(packet)?;
        self.last_send_time = self.time;
        Ok(())
    }

    /// Calls send_fn with every packet ready to send.
    ///
    /// In case we aren't fully connected, this could be resending challenge responses etc.
    /// If we are connected, it delegates to pickleback for messages packets.
    ///
    /// # Panics
    /// Only on internal logic errors
    pub fn visit_packets_to_send(&mut self, mut send_fn: impl FnMut(SocketAddr, &[u8])) {
        let Some(server_addr) = self.server_addr else {
            return;
        };

        let mut keepalive_needed = false;
        match self.state {
            ClientState::SelfInitiatedDisconnect if self.xor_salt.is_some() => {
                for _ in 0..10 {
                    if let Ok(header) = self.pickleback.next_packet_header(PacketType::Disconnect) {
                        let d = ProtocolPacket::Disconnect(DisconnectPacket {
                            header,
                            xor_salt: self.xor_salt.unwrap(),
                        });
                        // disconnecting, so ignore failures:
                        let _ = self.send(d);
                    }
                }
                log::debug!("Reseting Protocol Client state after disconnect");
                self.state_transition(ClientState::Disconnected(DisconnectReason::Normal));
            }

            ClientState::SendingConnectionRequest
                if self.proto_last_send < self.time - HANDSHAKE_RESEND_INTERVAL =>
            {
                if let Ok(header) = self
                    .pickleback
                    .next_packet_header(PacketType::ConnectionRequest)
                {
                    if self
                        .send(ProtocolPacket::ConnectionRequest(ConnectionRequestPacket {
                            protocol_version: PROTOCOL_VERSION,
                            client_salt: self.client_salt.unwrap(),
                            header,
                        }))
                        .is_ok()
                    {
                        self.proto_last_send = self.time;
                    }
                }
            }

            ClientState::SendingChallengeResponse
                if self.proto_last_send < self.time - HANDSHAKE_RESEND_INTERVAL
                    && self.xor_salt.is_some() =>
            {
                if let Ok(header) = self
                    .pickleback
                    .next_packet_header(PacketType::ConnectionChallengeResponse)
                {
                    if self
                        .send(ProtocolPacket::ConnectionChallengeResponse(
                            ConnectionChallengeResponsePacket {
                                header,
                                xor_salt: self.xor_salt.unwrap(),
                            },
                        ))
                        .is_ok()
                    {
                        self.proto_last_send = self.time;
                    }
                }
            }

            ClientState::Connected => {
                keepalive_needed = self.time - self.last_send_time >= KEEPALIVE_INTERVAL;
            }

            _ => {}
        }

        let sent = self
            .pickleback
            .visit_packets_to_send(|packet| send_fn(server_addr, packet));

        if sent == 0 && keepalive_needed {
            log::debug!(
                "KeepAlive needed. send_unacked:{} received_unacked:{} received_unacked_nonempt:{}",
                self.pickleback.sent_unacked_packets(),
                self.pickleback.received_unacked_packets(),
                self.pickleback.received_unacked_non_empty_packets()
            );
            // it's possible we can't, due to backpressure because of too many unacked packets
            // in flight, but in that case there's probably no hope for this connection anyway.
            if let Ok(header) = self.pickleback.next_packet_header(PacketType::KeepAlive) {
                let keepalive = ProtocolPacket::KeepAlive(KeepAlivePacket {
                    header,
                    client_index: 0,
                    xor_salt: self
                        .xor_salt
                        .expect("Should be an xor_salt in Connected state"),
                });
                let _ = self.send(keepalive);
            }
            self.pickleback.visit_packets_to_send(|packet| {
                send_fn(server_addr, packet);
            });
        }
    }

    /// Process received packet.
    ///
    /// # Errors
    /// * `PicklebackError::SocketAddrMismatch` - if packet from source other than server we connected to
    /// * `PicklebackError::InvalidPacket` - malformed packet
    pub fn receive(&mut self, packet: &[u8], source: SocketAddr) -> Result<(), PicklebackError> {
        let Some(server_addr) = self.server_addr else {
            return Ok(());
        };
        if server_addr != source {
            return Err(PicklebackError::SocketAddrMismatch);
        }
        let mut cur = Cursor::new(packet);
        let new_state = {
            let packet = read_packet(&mut cur)?;
            self.last_receive_time = self.time;
            log::trace!("client got >>> {packet:?}");
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
                ProtocolPacket::ConnectionDenied(ConnectionDeniedPacket { reason, .. }) => {
                    if self.state == ClientState::SendingConnectionRequest
                        || self.state == ClientState::SendingChallengeResponse
                    {
                        log::warn!("Connection was denied: {reason}");
                        self.server_salt = None;
                        self.xor_salt = None;
                        Some(ClientState::Disconnected(reason))
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
                    // totally normal to get some useless packets here because during a handshake
                    // the remote may send a second Challenge while our ChallengeResponse is still
                    // in flight, amongst other examples.
                    log::trace!(
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
