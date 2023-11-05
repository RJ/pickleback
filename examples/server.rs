extern crate bevy_reparate;
use bevy::prelude::*;
use bevy_reparate::*;

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins);
    app.add_plugins(ReparateServerPlugin);
    app.add_systems(Update, print_packets);
    app.run();
}

fn print_packets(mut ev: ResMut<Events<ReceivedPacket>>) {
    for ReceivedPacket {
        slot,
        received_at,
        payload,
    } in ev.drain()
    {
        info!("PRINT_PACKETS {slot} = {payload:?}");
    }
}
