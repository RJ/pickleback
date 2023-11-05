mod client;
mod reliable;
mod server;
pub use client::*;
pub use server::*;
// use anyhow::Result;
use bevy::prelude::*;
use reliable::*;
use serde::*;

use naia_socket_shared::{LinkConditionerConfig, SocketConfig};

pub fn naia_shared_config() -> SocketConfig {
    //let link_condition = None;
    let link_condition = Some(LinkConditionerConfig::average_condition());
    //    let link_condition = Some(LinkConditionerConfig {
    //        incoming_latency: 500,
    //        incoming_jitter: 1,
    //        incoming_loss: 0.0,
    //        incoming_corruption: 0.0
    //    });

    SocketConfig::new(link_condition, None)
}
