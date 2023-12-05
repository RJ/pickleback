extern crate pickleback;
// use std::collections::HashMap;
// use std::collections::HashSet;
// use std::collections::VecDeque;

// use log::*;
// use pickleback::prelude::*;
use pickleback::testing::*;

#[test]
fn protocol() {
    init_logger();
    // let channel = 0;
    let mut harness = ProtocolTestHarness::new(JitterPipeConfig::disabled());
    harness.client.connect("127.0.0.1:6000");
    harness.advance(0.1);
    harness.advance(0.1);
    harness.advance(0.1);
    harness.advance(0.1);
    harness.advance(0.1);
    harness.advance(0.1);
    harness.advance(0.1);
    log::warn!("ADVANCING SERVERV ONLY FOR TIMEOUT TEST");
    for _ in 0..4 {
        let dt = 2.0;
        harness.client.update(-dt);
        harness.advance(dt);
    }
    log::warn!("ADVANCING CLIENT ONLY FOR TIMEOUT TEST");
    for _ in 0..4 {
        let dt = 2.0;
        harness.server.update(-dt);
        harness.advance(dt);
    }
}
