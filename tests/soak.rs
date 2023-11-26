extern crate packeteer;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use log::*;
use packeteer::prelude::*;
use packeteer::testing::*;

/// How many messages to send during soak tests:
const NUM_TEST_MSGS: usize = 1000;
/// If doing reliable sending over lossy pipe, how many extra ticks to mop up straggling acks:
const NUM_EXTRA_ITERATIONS: usize = 100;
/// Random payload size is selected to generate up to this many fragments:
const MAX_FRAGMENTS: u32 = 10;

#[test]
fn soak_message_transmission() {
    init_logger();
    let channel = 0;
    let mut harness = TestHarness::new(JitterPipeConfig::disabled());

    // pre-generate test messages of varying sizes, many of which require fragmentation
    let test_msgs = (0..NUM_TEST_MSGS)
        .map(|_| random_payload_max_frags(MAX_FRAGMENTS))
        .collect::<Vec<_>>();

    let mut send_msgs_iter = test_msgs.iter();
    let mut in_flight_msgs = HashMap::<Vec<u8>, MessageId>::new();
    let mut sent_ids = VecDeque::new();

    'sending: while !test_msgs.is_empty() {
        // Send up to num_to-  NB: check MAX_FRAGMENTS * this is under ack limit.
        let num_to_send = rand::random::<usize>() % 7;
        let mut num_sent = 0;

        for _ in 0..num_to_send {
            let Some(msg) = send_msgs_iter.next() else {
                if num_sent == 0 {
                    break 'sending;
                } else {
                    break;
                }
            };
            num_sent += 1;
            let msg_id = harness.server.send_message(channel, msg.as_ref()).unwrap();
            in_flight_msgs.insert(msg.clone(), msg_id);
            // info!("{msg_id:?} PAYLOAD(len:{}) = {msg:?}", msg.len());
            sent_ids.push_back(msg_id);
        }

        harness.advance(0.03);

        let client_received_messages = harness
            .client
            .drain_received_messages(channel)
            .collect::<Vec<_>>();

        // acks not guaranteed to be reported in same order they were sent - using hashset.
        let acks = harness
            .server
            .drain_message_acks(channel)
            .collect::<HashSet<_>>();

        assert_eq!(
            client_received_messages.len(),
            num_sent,
            "didn't receive all the messages sent this tick"
        );

        for recv_msg in client_received_messages.iter() {
            let rec = recv_msg.payload_to_owned();
            if let Some(expected_id) = in_flight_msgs.get(&rec) {
                assert_eq!(*expected_id, recv_msg.id(), "ids don't match");
            } else {
                panic!("payload not found in sent msgs for {recv_msg:?}, payload: {rec:?}");
            }
            assert!(
                acks.contains(&sent_ids.pop_front().unwrap()),
                "ack mismatch"
            );
        }
    }
}

#[test]
fn soak_reliable_message_transmission_with_terrible_network() {
    init_logger();
    let channel = 1;
    let mut harness = TestHarness::new(JitterPipeConfig::terrible());

    let mut test_msgs = Vec::new();
    (0..NUM_TEST_MSGS).for_each(|_| test_msgs.push(random_payload_max_frags(MAX_FRAGMENTS)));

    let mut unacked_sent_msg_ids = Vec::new();

    let mut client_received_messages = Vec::new();

    let mut i = 0;
    while i < NUM_TEST_MSGS + NUM_EXTRA_ITERATIONS {
        if let Some(msg) = test_msgs.get(i) {
            let size = msg.len();
            match harness.server.send_message(channel, msg.as_ref()) {
                Ok(msg_id) => {
                    trace!("💌💌 Sending message {i}/{NUM_TEST_MSGS}, size {size},  msg_id: {msg_id:?}");
                    unacked_sent_msg_ids.push(msg_id);
                    i += 1;
                }
                Err(PacketeerError::Backpressure(Backpressure::TooManyPending)) => {
                    warn!("Backpressure");
                }
                Err(e) => panic!("Unhandled send error {e:?}"),
            }
        } else {
            i += 1;
        }

        let stats = harness.advance(0.051);
        info!("{stats:?}");

        let acked_ids = harness.collect_server_acks(channel);
        if !acked_ids.is_empty() {
            unacked_sent_msg_ids.retain(|id| !acked_ids.contains(id));
            // trace!(
            //     "👍 Server got ACKs: {acked_ids:?} still need: {} : {unacked_sent_msg_ids:?}",
            //     unacked_sent_msg_ids.len()
            // );
        }

        client_received_messages.extend(harness.client.drain_received_messages(channel));
    }

    assert_eq!(
        Vec::<MessageId>::new(),
        unacked_sent_msg_ids,
        "server is missing acks for these messages"
    );

    // with enough extra iterations, resends should have ensured everything was received.
    assert_eq!(client_received_messages.len(), test_msgs.len());

    assert_eq!(harness.server.packet_loss(), 2.0);
}
