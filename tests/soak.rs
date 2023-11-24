extern crate packeteer;
use std::collections::HashSet;
use std::collections::VecDeque;

use log::*;
use packeteer::prelude::*;
use packeteer::test_utils::*;

/// How many messages to send during soak tests:
const NUM_TEST_MSGS: usize = 100000;
/// If doing reliable sending over lossy pipe, how many extra ticks to mop up straggling acks:
const NUM_EXTRA_ITERATIONS: usize = 100;

#[test]
fn soak_message_transmission() {
    init_logger();
    let channel = 0;
    let mut harness = TestHarness::new(JitterPipeConfig::disabled());

    // pre-generate test messages of varying sizes, many of which require fragmentation
    let test_msgs = (0..NUM_TEST_MSGS)
        .map(|_| {
            // max of 6 frags and 5x in flight = 30 packets, with ack limit of 32.
            let size = 1 + rand::random::<u32>() % (1024 * 6);
            random_payload(size)
        })
        .collect::<Vec<_>>();

    let mut send_msgs_iter = test_msgs.iter();
    // let mut recv_msgs_iter = test_msgs.iter();
    let mut unseen_msgs = HashSet::<Vec<u8>>::from_iter(test_msgs.iter().cloned());

    let mut sent_ids = VecDeque::new();

    'sending: while !test_msgs.is_empty() {
        // Send up to num_to-
        let num_to_send = rand::random::<usize>() % 5;
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

        log::info!("XXX acks: {acks:?} send_ids: {sent_ids:?}");

        for recv_msg in client_received_messages.iter() {
            let rec = recv_msg.payload_to_owned();
            assert!(
                unseen_msgs.remove(&rec),
                "payload not found in sent msgs for {recv_msg:?}"
            );
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
    (0..NUM_TEST_MSGS)
        .for_each(|_| test_msgs.push(random_payload(rand::random::<u32>() % (1024 * 16))));

    let mut unacked_sent_msg_ids = Vec::new();

    let mut client_received_messages = Vec::new();

    for i in 0..(NUM_TEST_MSGS + NUM_EXTRA_ITERATIONS) {
        if let Some(msg) = test_msgs.get(i) {
            let size = msg.len();
            let msg_id = harness.server.send_message(channel, msg.as_ref()).unwrap();
            trace!("💌💌 Sending message {i}/{NUM_TEST_MSGS}, size {size},  msg_id: {msg_id}");
            unacked_sent_msg_ids.push(msg_id);
        }

        let stats = harness.advance(0.051);
        info!("{stats:?}");

        let acked_ids = harness.collect_server_acks(channel);
        if !acked_ids.is_empty() {
            unacked_sent_msg_ids.retain(|id| !acked_ids.contains(id));
            trace!(
                "👍 Server got ACKs: {acked_ids:?} still need: {} : {unacked_sent_msg_ids:?}",
                unacked_sent_msg_ids.len()
            );
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
}
