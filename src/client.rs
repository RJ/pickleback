use std::{net::SocketAddr, time::Duration};

use bevy::{app::AppLabel, prelude::*, utils::Instant};

use naia_client_socket::{PacketReceiver, PacketSender, ServerAddr, Socket};

#[derive(Resource)]
pub struct ReparateClient {
    packet_sender: Box<dyn PacketSender>,
    packet_receiver: Box<dyn PacketReceiver>,
    // endpoint
}

#[derive(Event)]
struct OutboundPacket {
    payload: Vec<u8>,
}

pub struct ReparateClientPlugin;

impl Plugin for ReparateClientPlugin {
    fn build(&self, app: &mut App) {
        let shared_config = crate::naia_shared_config();
        let (packet_sender, packet_receiver) =
            Socket::connect("http://127.0.0.1:14191", &shared_config);
        let client = ReparateClient {
            packet_receiver,
            packet_sender,
        };
        app.insert_resource(client);
        app.init_resource::<Events<OutboundPacket>>();
        app.add_systems(PreUpdate, Self::read_packets);
        app.add_systems(PostUpdate, Self::send_packets);
        app.add_systems(FixedUpdate, send_periodic_packet);
    }
}

fn send_periodic_packet(mut ev: EventWriter<OutboundPacket>) {
    let payload = Vec::from("PING");
    let packet = OutboundPacket { payload };
    ev.send(packet);
}

impl ReparateClientPlugin {
    fn send_packets(client: ResMut<ReparateClient>, mut ev: ResMut<Events<OutboundPacket>>) {
        for OutboundPacket { payload } in ev.drain() {
            match client.packet_sender.send(payload.as_slice()) {
                Ok(()) => {}
                Err(error) => {
                    info!("Client Send Error: {}", error);
                }
            }
        }
    }
    fn read_packets(mut client: ResMut<ReparateClient>) {
        loop {
            match client.packet_receiver.receive() {
                Ok(None) => {
                    break;
                }
                Ok(Some(packet)) => {
                    info!("Received: {packet:?}");
                    //
                }
                Err(err) => {
                    error!("Receive err: {err:?}");
                    break;
                }
            }
        }
    }
}
