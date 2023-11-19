mod reliable;

use bytes::{Buf, Bytes, BytesMut};
use log::*;
use reliable::*;
use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    mem::take,
};
mod message;
use message::*;
pub mod channel;
use channel::*;
pub mod jitter_pipe;
mod message_reassembler;
mod test_utils;
use message_reassembler::*;
mod dispatcher;
use dispatcher::*;
pub use jitter_pipe::*;

pub struct ReceivedMessage {
    /// ids on received msgs are used internally for uniqueness and ordering
    pub id: MessageId,
    pub channel: u8,
    pub payload: Bytes,
}

impl std::fmt::Debug for ReceivedMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ReceivedMessage{{id: {}, channel: {}, payload_len: {}}}",
            self.id,
            self.channel,
            self.payload.len()
        )
    }
}

pub struct Packeteer {
    endpoint: Endpoint,
    time: f64,
    dispatcher: MessageDispatcher,
    channels: ChannelList,
}

/// Represents one end of a datagram stream between two peers, one of which is the server.
///
/// ultimately probably want channels, IDed by a u8. then we can have per-channel settings.
/// eg ordering guarantees, reliability of messages, retransmit time, etc.
///
impl Packeteer {
    pub fn new(time: f64) -> Self {
        let endpoint_config = EndpointConfig {
            max_payload_size: 1024,
            ..Default::default()
        };
        let endpoint = Endpoint::new(endpoint_config, time);
        let mut channels = ChannelList::default();
        channels.put(Box::new(UnreliableChannel::new(0, time)));
        channels.put(Box::new(ReliableChannel::new(1, time)));
        Self {
            endpoint,
            time,
            dispatcher: MessageDispatcher::default(),
            channels,
        }
    }

    // used by tests
    #[allow(dead_code)]
    pub(crate) fn channels_mut(&mut self) -> &mut ChannelList {
        &mut self.channels
    }

    pub fn drain_packets_to_send(
        &mut self,
    ) -> std::collections::vec_deque::Drain<'_, bytes::Bytes> {
        self.write_packets_to_send();
        self.endpoint.drain_packets_to_send()
    }

    pub fn drain_received_messages(&mut self, channel: u8) -> std::vec::Drain<'_, ReceivedMessage> {
        self.dispatcher.drain_received_messages(channel)
    }

    pub fn drain_message_acks(&mut self, channel: u8) -> std::vec::Drain<'_, MessageId> {
        self.dispatcher.drain_message_acks(channel)
    }

    /// enqueue a message to be sent in a packet.
    /// messages get coalesced into packets.
    pub fn send_message(&mut self, channel: u8, message_payload: &[u8]) -> MessageId {
        let channel = self.channels.get_mut(channel).expect("No such channel");
        self.dispatcher
            .add_message_to_channel(channel, message_payload)
    }

    // when creating the messages, we want one big BytesMut?? with views into it, refcounted so
    // once no more messages are alive, it's cleaned up? then we can do a large contiguous allocation
    // for lots of small message buffers..
    // otherwise it's fragmenty af
    // it's almost an arena allocator cleared per frame, but some messages might not be sent until next frame,
    // and reliables need to stick around even longer..
    //
    //
    fn write_packets_to_send(&mut self) {
        // info!("write packets.");
        let mut sent_something = false;

        let mut message_handles_in_packet = Vec::<MessageHandle>::new();
        let max_packet_size = self.endpoint.config().max_packet_size;
        let mut packet = BytesMut::with_capacity(max_packet_size);
        let mut remaining_space = max_packet_size;
        // definitely scope to optimise these nested loops..
        // hopefully never sending too many packets per tick though, so maybe fine.
        while self.channels.any_with_messages_to_send() {
            // info!("any with msg to send");
            // for all channels with messages to send:
            'non_empty_channels: while let Some(channel) = self.channels.all_non_empty_mut().next()
            {
                let mut any_found = false;
                while let Some(msg) = channel.get_message_to_write_to_a_packet(remaining_space) {
                    any_found = true;
                    // trace!("* Writing {msg:?} to packet buffer..");
                    msg.write(&mut packet)
                        .expect("writing to a buffer shouldn't fail");
                    message_handles_in_packet.push(MessageHandle {
                        id: msg.id(),
                        frag_index: msg.fragment().map(|f| f.index),
                        channel: channel.id(),
                    });
                    remaining_space = max_packet_size - packet.len();
                    if remaining_space < 3 {
                        break 'non_empty_channels;
                    }
                }
                if !any_found {
                    break;
                }
            }
            if remaining_space == max_packet_size {
                continue;
            }
            // send packet.
            let final_packet = packet.freeze();
            packet = BytesMut::with_capacity(max_packet_size);
            remaining_space = max_packet_size;
            match self.endpoint.send(final_packet) {
                Ok(handle) => {
                    sent_something = true;
                    // info!("Sending packet containing msg ids: {message_handles_in_packet:?}, in packet seq {handle:?}");
                    self.dispatcher
                        .set_packet_message_handles(handle, take(&mut message_handles_in_packet));
                }
                Err(err) => {
                    error!("Err sending coalesced packet {err:?}");
                }
            }
        }

        // if no Messages to send, we'll still send an empty-payload packet, so that
        // acks are transmitted.
        // sending one empty packet per tick is fine.. right? what about uncapped headless server?
        if !sent_something {
            // info!("No msgs. Sending empty packet, jsut for acks");
            self.endpoint.send(Bytes::new()).unwrap();
        }
    }

    pub fn update(&mut self, dt: f64) {
        self.time += dt;
        self.endpoint.update(dt);
        // updating time for channels may result in reliable channels enqueuing messages
        // that need to be retransmitted.
        for channel in self.channels.all_mut() {
            channel.update(dt);
        }
    }

    /// a datagram was received, written to a Bytes, and passed in here for processing
    /// since we don't do the actual networking ourselves..
    ///
    /// could split this up by not returning ReceivedPacets, but writing to a queue
    /// so we could process incoming in PreUpdate, but only process the queue of received in Fixed?
    pub fn process_incoming_packet(&mut self, packet: Bytes) {
        match self.endpoint.receive(packet) {
            Ok(ReceivedPacket {
                handle: _,
                payload,
                acks,
            }) => {
                // info!("Received msg {handle:?}");
                for acked_handle in &acks {
                    self.dispatcher
                        .acked_packet(acked_handle, &mut self.channels);
                }
                let mut reader = payload;
                while reader.remaining() > 0 {
                    match Message::parse(&mut reader) {
                        Ok(msg) => {
                            // info!("Parsed msg: {msg:?}");
                            if let Some(channel) = self.channels.get_mut(msg.channel()) {
                                if channel.accepts_message(&msg) {
                                    self.dispatcher.process_received_message(msg);
                                } else {
                                    warn!("Channel rejects message id {msg:?}");
                                }
                            }
                        }
                        Err(err) => {
                            error!("Error parsing messages from packet payload: {err:?}");
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                error!("incoming packet error {err:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::jitter_pipe::{JitterPipe, JitterPipeConfig};

    use super::*;
    use test_utils::*;
    // explicit import to override bevy
    // use log::{debug, error, info, trace, warn};

    #[test]
    fn small_unfrag_messages() {
        crate::test_utils::init_logger();
        let channel = 0;
        let mut harness = TestHarness::new(JitterPipeConfig::disabled());

        let msg1 = b"Hello";
        let msg2 = b"world";
        let msg3 = b"!";
        let id1 = harness.server.send_message(channel, msg1);
        let id2 = harness.server.send_message(channel, msg2);
        let id3 = harness.server.send_message(channel, msg3);

        harness.advance(0.1);

        let received_messages = harness
            .client
            .drain_received_messages(channel)
            .collect::<Vec<_>>();
        assert_eq!(received_messages[0].payload.as_ref(), msg1);
        assert_eq!(received_messages[1].payload.as_ref(), msg2);
        assert_eq!(received_messages[2].payload.as_ref(), msg3);

        assert_eq!(
            vec![id1, id2, id3],
            harness
                .server
                .drain_message_acks(channel)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn frag_message() {
        crate::test_utils::init_logger();
        let channel = 0;
        let mut harness = TestHarness::new(JitterPipeConfig::disabled());

        let mut msg = Vec::new();
        msg.extend_from_slice(&[65; 1024]);
        msg.extend_from_slice(&[66; 1024]);
        msg.extend_from_slice(&[67; 100]);

        let msg_id = harness.server.send_message(channel, msg.as_ref());

        harness.advance(0.1);

        let received_messages = harness
            .client
            .drain_received_messages(channel)
            .collect::<Vec<_>>();
        assert_eq!(received_messages.len(), 1);
        assert_eq!(received_messages[0].payload, msg);

        // client should have sent acks back to server
        assert_eq!(
            vec![msg_id],
            harness
                .server
                .drain_message_acks(channel)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reject_duplicate_messages() {
        crate::test_utils::init_logger();
        let channel = 0;
        let mut harness = TestHarness::new(JitterPipeConfig::disabled());
        let payload = b"hello";
        harness
            .server
            .channels_mut()
            .get_mut(channel)
            .unwrap()
            .enqueue_message(123, payload, Fragmented::No);
        harness
            .server
            .channels_mut()
            .get_mut(channel)
            .unwrap()
            .enqueue_message(123, payload, Fragmented::No);
        harness.advance(1.);
        assert_eq!(harness.client.drain_received_messages(channel).len(), 1);
        // let msg =
    }

    // TODO this isn't testing having multiple messages in flight yet? batch the sending and receiving?
    #[test]
    fn soak_message_transmission() {
        crate::test_utils::init_logger();
        let channel = 0;
        let mut harness = TestHarness::new(JitterPipeConfig::disabled());

        // TODO crashes when msg id rolls around
        for _ in 0..1000 {
            let size = rand::random::<u32>() % (1024 * 16);
            let msg = random_payload(size);
            let msg_id = harness.server.send_message(channel, msg.as_ref());
            info!("💌 Sending message of size {size}, msg_id: {msg_id}");

            harness.advance(0.03);

            let client_received_messages = harness
                .client
                .drain_received_messages(channel)
                .collect::<Vec<_>>();
            assert_eq!(client_received_messages.len(), 1);
            assert_eq!(client_received_messages[0].payload, msg);

            assert_eq!(
                vec![msg_id],
                harness
                    .server
                    .drain_message_acks(channel)
                    .collect::<Vec<_>>()
            );
        }
    }

    // test what happens in a reliable channel when one of the fragments isn't delivered.
    // should resend after a suitable amount of time.
    #[test]
    fn retransmission() {
        let channel = 1;
        let mut harness = TestHarness::new(JitterPipeConfig::disabled());
        // big enough to require 2 packets
        let payload = random_payload(1800);
        let id = harness.server.send_message(channel, payload.as_ref());
        // drop second packet (index 1), which will be the second of the two fragments.
        harness.advance_with_server_outbound_drops(0.05, vec![1]);
        assert!(harness.collect_client_messages(channel).is_empty());
        assert!(harness.collect_server_acks(channel).is_empty());
        // retransmit not ready yet
        harness.advance(0.01);
        assert!(harness.collect_client_messages(channel).is_empty());
        assert!(harness.collect_server_acks(channel).is_empty());
        // should retransmit
        harness.advance(0.09001); // retransmit time of 0.1 reached
        assert_eq!(harness.collect_client_messages(channel).len(), 1);
        assert_eq!(vec![id], harness.collect_server_acks(channel));
        // ensure server finished sending:
        harness.advance(1.0);
        // this is testing that the server is only transmitting one small "empty" packet (just headers)
        let to_send = harness.server.drain_packets_to_send().collect::<Vec<_>>();
        assert_eq!(1, to_send.len());
        // both our fragments are def bigger than 50:
        assert!(to_send[0].len() < 50);
    }

    const NUM_TEST_MSGS: usize = 1000;
    // extras are to ensure and resends / acks actually can be retransmitted
    const NUM_EXTRA_ITERATIONS: usize = 100;

    #[test]
    fn soak_message_transmission_with_jitter_pipe() {
        crate::test_utils::init_logger();
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
                let msg_id = harness.server.send_message(channel, msg.as_ref());
                info!("💌💌 Sending message {i}/{NUM_TEST_MSGS}, size {size},  msg_id: {msg_id}");
                unacked_sent_msg_ids.push(msg_id);
            }

            let stats = harness.advance(0.051);
            info!("{stats:?}");

            let acked_ids = harness.collect_server_acks(channel);
            if !acked_ids.is_empty() {
                unacked_sent_msg_ids.retain(|id| !acked_ids.contains(id));
                info!(
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
}
