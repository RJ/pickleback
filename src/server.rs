use std::{net::SocketAddr, time::Duration};

use bevy::{prelude::*, utils::Instant};

use naia_server_socket::{
    NaiaServerSocketError, PacketReceiver, PacketSender, ServerAddrs, Socket,
};

pub type SlotNumber = u8;

#[derive(Resource)]
pub struct ServerChannels {
    pub packet_sender: Box<dyn PacketSender>,
    pub packet_receiver: Box<dyn PacketReceiver>,
}

pub struct ConnectedClient {
    handle: ClientHandle,
    address: SocketAddr,
    last_packet_time: Instant,
    salt: u32,
    // endpoint things
}

impl ConnectedClient {
    fn handle(&self) -> &ClientHandle {
        &self.handle
    }
}

#[derive(Debug, Copy, Clone)]
pub struct ClientHandle {
    slot: SlotNumber,
    salt: u32,
}

impl ConnectedClient {
    fn set_last_packet_time(&mut self, last_packet_time: Instant) {
        self.last_packet_time = last_packet_time;
    }
}

#[derive(Resource, Default)]
pub struct ServerSlots {
    slots: [Option<ConnectedClient>; 32],
    next_salt: u32,
}

#[derive(Event)]
pub enum NetworkEvent {
    ClientConnected(ClientHandle),
    ClientDisconnected(ClientHandle),
}

// TODO should put clients into a pending state until we get a valid own-protocol packet, checking
// protocol version, etc

impl ServerSlots {
    fn connected_clients(&self) -> impl Iterator<Item = &'_ ConnectedClient> {
        let x = self.slots.iter().filter_map(|o| o.as_ref());
        x
    }
    fn get(&self, handle: ClientHandle) -> Option<&ConnectedClient> {
        if let Some(cc) = &self.slots[handle.slot as usize] {
            if cc.salt == handle.salt {
                return Some(cc);
            }
        }
        None
    }
    // fn get_or_insert_client(&mut self, address: Sco)
    fn slot_in_new_client(
        &mut self,
        address: SocketAddr,
        instant: Instant,
    ) -> Option<ClientHandle> {
        for i in 0..self.slots.len() {
            if self.slots[i].is_some() {
                continue;
            }
            let handle = ClientHandle {
                slot: i as SlotNumber,
                salt: self.next_salt,
            };
            let connected_client = ConnectedClient {
                handle,
                address,
                last_packet_time: instant,
                salt: handle.salt,
            };
            self.next_salt += 1;
            self.slots[i] = Some(connected_client);
            return Some(handle);
        }
        // no slots free
        None
    }
    fn get_connected_client_mut(&mut self, address: SocketAddr) -> Option<&mut ConnectedClient> {
        let mut found = None;
        for i in 0..self.slots.len() {
            if let Some(slot) = &self.slots[i] {
                if slot.address == address {
                    found = Some(i);
                    break;
                }
            }
        }
        found.map(|i| self.slots.get_mut(i).unwrap().as_mut().unwrap())
    }
}

pub struct ReparateServerPlugin;

#[derive(Event)]
pub struct ReceivedPacket {
    pub client_handle: ClientHandle,
    pub received_at: Instant,
    pub payload: Vec<u8>,
}

#[derive(Event)]
struct OutboundPacket {
    client_handle: ClientHandle,
    payload: Vec<u8>,
}

impl Plugin for ReparateServerPlugin {
    fn build(&self, app: &mut App) {
        let server_address = ServerAddrs::new(
            "127.0.0.1:14191"
                .parse()
                .expect("could not parse Session address/port"),
            // IP Address to listen on for UDP WebRTC data channels
            "127.0.0.1:14192"
                .parse()
                .expect("could not parse WebRTC data address/port"),
            // The public WebRTC IP address to advertise
            "http://127.0.0.1:14192",
        );
        let shared_config = crate::naia_shared_config();
        let (packet_sender, packet_receiver) = Socket::listen(&server_address, &shared_config);
        let server_channels = ServerChannels {
            packet_receiver,
            packet_sender,
        };
        app.insert_resource(server_channels);
        app.insert_resource(ServerSlots::default());
        app.init_resource::<Events<NetworkEvent>>();
        app.init_resource::<Events<ReceivedPacket>>();
        app.init_resource::<Events<OutboundPacket>>();
        app.add_systems(PreUpdate, Self::read_packets);
        app.add_systems(PostUpdate, Self::send_packets);

        app.add_systems(FixedUpdate, Self::periodic_send);
    }
}

impl ReparateServerPlugin {
    fn periodic_send(
        mut server_channels: ResMut<ServerChannels>,
        mut server_slots: ResMut<ServerSlots>,
        mut ev: EventWriter<OutboundPacket>,
    ) {
        let payload = Vec::from("HELLO".as_bytes());
        for cc in server_slots.connected_clients() {
            ev.send(OutboundPacket {
                client_handle: *cc.handle(),
                payload: payload.clone(),
            })
        }
    }

    fn send_packets(
        mut server_channels: ResMut<ServerChannels>,
        mut server_slots: ResMut<ServerSlots>,
        mut ev: ResMut<Events<OutboundPacket>>,
        mut net_ev: EventWriter<NetworkEvent>,
    ) {
        let now = Instant::now();
        // send queued outbound packets
        for OutboundPacket {
            client_handle,
            payload,
        } in ev.drain()
        {
            if let Some(cc) = server_slots.get(client_handle) {
                match server_channels
                    .packet_sender
                    .send(&cc.address, payload.as_slice())
                {
                    Ok(()) => {
                        info!("sent packet to {client_handle:?}");
                    }
                    Err(err) => {
                        warn!("server send err {err:?}");
                    }
                }
                // get address, use channel sender
            } else {
                warn!("Dropping outbound packet for empty slot {client_handle:?}");
            }
        }
        // check for client timeout due to no packets received recently
        let timed_out = server_slots
            .connected_clients()
            .filter(|cc| now - cc.last_packet_time > Duration::from_millis(5000))
            .map(|cc| cc.handle)
            .collect::<Vec<_>>();

        for handle in timed_out {
            error!("Client timeout: {handle:?}");
            server_slots.slots[handle.slot as usize] = None;
            net_ev.send(NetworkEvent::ClientDisconnected(handle));
            continue;
        }
    }

    fn read_packets(
        mut server_channels: ResMut<ServerChannels>,
        mut ev: EventWriter<ReceivedPacket>,
        mut slots: ResMut<ServerSlots>,
        mut net_ev: EventWriter<NetworkEvent>,
    ) {
        loop {
            match server_channels.packet_receiver.receive() {
                Ok(Some((address, payload))) => {
                    let received_at = Instant::now();
                    // if there isn't a connected client already, create one
                    let client_handle = if let Some(cc) = slots.get_connected_client_mut(address) {
                        cc.set_last_packet_time(received_at);
                        net_ev.send(NetworkEvent::ClientConnected(cc.handle));
                        info!("Recv[{}]> {address:?} {payload:?}", cc.handle.slot);
                        cc.handle
                    } else if let Some(handle) = slots.slot_in_new_client(address, received_at) {
                        // TODO publish a "new client connected" msg
                        info!("Recv[NEW: {handle:?}]> {address:?} {payload:?}");
                        handle
                    } else {
                        // TODO respond with "server full"
                        error!("Connection attempt from {address:?} but no slots");
                        continue;
                    };

                    let p = ReceivedPacket {
                        received_at,
                        client_handle,
                        payload: Vec::from(payload), // TODO lazy clone.
                    };
                    ev.send(p);
                }
                Ok(None) => {
                    break;
                }
                Err(NaiaServerSocketError::SendError(destination)) => {
                    error!("Can't send to adddress: {destination:?} - drop client?");
                    break;
                }
                Err(NaiaServerSocketError::Wrapped(error)) => {
                    error!("Server Error: {:?}", error);
                    // hopefully an error means no more packets to read this tick,
                    // otherwise we'd be introducing a tick delay on processing here..
                    break;
                }
            }
        }
    }
}
