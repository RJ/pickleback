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
    slot: SlotNumber,
    address: SocketAddr,
    last_packet_time: Instant,
    // endpoint things
}

impl ConnectedClient {
    fn set_last_packet_time(&mut self, last_packet_time: Instant) {
        self.last_packet_time = last_packet_time;
    }
}

#[derive(Resource, Default)]
pub struct ServerSlots {
    slots: [Option<ConnectedClient>; 32],
}

impl ServerSlots {
    // fn get_or_insert_client(&mut self, address: Sco)
    fn slot_in_new_client(&mut self, address: SocketAddr, instant: Instant) -> Option<SlotNumber> {
        for i in 0..self.slots.len() {
            if self.slots[i].is_some() {
                continue;
            }
            let connected_client = ConnectedClient {
                slot: i as SlotNumber,
                address,
                last_packet_time: instant,
            };
            self.slots[i] = Some(connected_client);
            return Some(i as SlotNumber);
        }
        // no slots free
        None
    }
    fn get_connected_client_mut(&mut self, address: SocketAddr) -> Option<&mut ConnectedClient> {
        // yuk.. find a more elegant way
        let mut found = usize::MAX;
        for i in 0..self.slots.len() {
            if let Some(slot) = &self.slots[i] {
                if slot.address == address {
                    found = i;
                    break;
                }
            }
        }
        if found == usize::MAX {
            None
        } else {
            self.slots.get_mut(found).unwrap().as_mut()
        }
    }
}

pub struct ReparateServerPlugin;

#[derive(Event)]
pub struct ReceivedPacket {
    pub slot: SlotNumber,
    pub received_at: Instant,
    pub payload: Vec<u8>,
}

#[derive(Event)]
struct OutboundPacket {
    slot: SlotNumber,
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
        app.init_resource::<Events<ReceivedPacket>>();
        app.init_resource::<Events<OutboundPacket>>();
        app.add_systems(PreUpdate, Self::read_packets);
        app.add_systems(PostUpdate, Self::send_packets);
    }
}

impl ReparateServerPlugin {
    fn send_packets(
        mut server_channels: ResMut<ServerChannels>,
        mut server_slots: ResMut<ServerSlots>,
        mut ev: ResMut<Events<OutboundPacket>>,
    ) {
        let now = Instant::now();
        // send queued outbound packets
        for OutboundPacket { slot, payload } in ev.drain() {
            if let Some(cc) = server_slots.slots.get(slot as usize).unwrap() {
                match server_channels
                    .packet_sender
                    .send(&cc.address, payload.as_slice())
                {
                    Ok(()) => {
                        info!("sent packet to {slot}");
                    }
                    Err(err) => {
                        warn!("server send err {err:?}");
                    }
                }
                // get address, use channel sender
            } else {
                warn!("Dropping outbound packet for empty slot {slot}");
            }
        }
        // check for client timeout due to no packets received recently
        for i in 0..server_slots.slots.len() {
            if let Some(cc) = &server_slots.slots[i] {
                let age = now - cc.last_packet_time;
                if age > Duration::from_millis(3000) {
                    // error!("Client timeout: {i} {:?}", cc.address);
                    // TODO publish client disconnect event
                    // server_slots.slots[i] = None;
                    continue;
                }
            }
        }
    }

    fn read_packets(
        mut server_channels: ResMut<ServerChannels>,
        mut ev: EventWriter<ReceivedPacket>,
        mut slots: ResMut<ServerSlots>,
    ) {
        loop {
            match server_channels.packet_receiver.receive() {
                Ok(Some((address, payload))) => {
                    let received_at = Instant::now();
                    // if there isn't a connected client already, create one
                    let slot = if let Some(cc) = slots.get_connected_client_mut(address) {
                        cc.set_last_packet_time(received_at);
                        info!("Recv[{}]> {address:?} {payload:?}", cc.slot);
                        cc.slot
                    } else if let Some(slot_num) = slots.slot_in_new_client(address, received_at) {
                        // TODO publish a "new client connected" msg
                        info!("Recv[NEW: {slot_num}]> {address:?} {payload:?}");
                        slot_num
                    } else {
                        // TODO respond with "server full"
                        error!("Connection attempt from {address:?} but no slots");
                        continue;
                    };

                    let p = ReceivedPacket {
                        received_at,
                        slot,
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
