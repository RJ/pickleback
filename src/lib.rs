mod reliable;
use std::fmt::Debug;

use __private::PhantomData;
use aeronet::{
    ClientId, ClientTransportPlugin, FromClient, FromServer, LocalClientConnected,
    LocalClientDisconnected, RemoteClientConnected, RemoteClientDisconnected,
    ServerTransportPlugin, ToClient, ToServer, TryFromBytes, TryIntoBytes,
};
use aeronet_channel::{ChannelTransportClient, ChannelTransportServer};
// use anyhow::Result;
use bevy::prelude::*;
use reliable::*;
use serde::*;

// with channel transport, you have to pass in the client transport after getting it from server
#[derive(Default)]
pub struct ReparateClient<C2S: aeronet::Message + Clone, S2C: aeronet::Message + Debug> {
    _phantom_c2s: PhantomData<C2S>,
    _phantom_s2c: PhantomData<S2C>,
}
// impl<C2S: aeronet::Message + Clone, S2C: aeronet::Message + Debug> ReparateClient<C2S, S2C> {
//     pub fn new() -> Self {
//         Self {
//             _phantom_c2s: PhantomData::default(),
//             _phantom_s2c: PhantomData::default(),
//         }
//     }
// }

impl<C2S: aeronet::Message + Clone, S2C: aeronet::Message + Debug> Plugin
    for ReparateClient<C2S, S2C>
{
    fn build(&self, app: &mut App) {
        app.add_plugins(ClientTransportPlugin::<
            _,
            _,
            ChannelTransportClient<C2S, S2C>,
        >::default());
        app.add_systems(Update, Self::handle_client);
    }
}

impl<C2S: aeronet::Message + Clone, S2C: aeronet::Message + Debug> ReparateClient<C2S, S2C> {
    fn handle_client(
        mut connected: EventReader<LocalClientConnected>,
        mut disconnected: EventReader<LocalClientDisconnected>,
        mut recv: EventReader<FromServer<S2C>>,
    ) {
        for LocalClientConnected in connected.iter() {
            info!("Connected to server!");
        }

        for LocalClientDisconnected(reason) in disconnected.iter() {
            info!(
                "Disconnected from server! {reason:?} = {}",
                aeronet::error::as_pretty(reason)
            );
        }

        for FromServer(msg) in recv.iter() {
            info!("received> '{msg:?}'");
        }
    }
}
