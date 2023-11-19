use bytes::{Buf, Bytes, BytesMut};
use log::*;
use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    mem::take,
};
mod channel;
mod config;
mod dispatcher;
mod error;
mod jitter_pipe;
mod message;
mod message_reassembler;
mod packet;
mod sequence_buffer;
mod test_utils;
mod tracking;

use channel::*;
use config::*;
use dispatcher::*;
use error::*;
use jitter_pipe::*;
use message::*;
use message_reassembler::*;
use packet::*;
use sequence_buffer::*;
use tracking::*;

pub mod prelude {
    pub use super::config::EndpointConfig;
    pub use super::error::ReliableError;
    pub use super::jitter_pipe::JitterPipeConfig;
    pub use super::tracking::EndpointCounters;
    pub use super::Packeteer;
    pub use super::ReceivedMessage;
}

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

/// returned from send - contains packet seqno
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct SentHandle(u16);

pub struct Packeteer {
    time: f64,
    rtt: f32,
    config: EndpointConfig,
    sequence: u16,
    dispatcher: MessageDispatcher,
    channels: ChannelList,
    sent_buffer: SequenceBuffer<SentData>,
    recv_buffer: SequenceBuffer<RecvData>,
    counters: EndpointCounters,
    outbox: VecDeque<Bytes>,
}

impl Default for Packeteer {
    fn default() -> Self {
        Self::new(EndpointConfig::default(), 1.0)
    }
}

/// Represents one end of a datagram stream between two peers, one of which is the server.
///
/// ultimately probably want channels, IDed by a u8. then we can have per-channel settings.
/// eg ordering guarantees, reliability of messages, retransmit time, etc.
///
impl Packeteer {
    pub fn new(config: EndpointConfig, time: f64) -> Self {
        let mut channels = ChannelList::default();
        channels.put(Box::new(UnreliableChannel::new(0, time)));
        channels.put(Box::new(ReliableChannel::new(1, time)));
        Self {
            time,
            rtt: 0.0,
            config: config.clone(),
            sequence: 0,
            sent_buffer: SequenceBuffer::with_capacity(config.sent_packets_buffer_size),
            recv_buffer: SequenceBuffer::with_capacity(config.received_packets_buffer_size),
            dispatcher: MessageDispatcher::default(),
            counters: EndpointCounters::default(),
            outbox: VecDeque::new(),
            channels,
        }
    }

    // used by tests
    #[allow(dead_code)]
    pub(crate) fn channels_mut(&mut self) -> &mut ChannelList {
        &mut self.channels
    }

    /// draining iterator over packets in the outbox that we need to send over the network
    pub fn drain_packets_to_send(
        &mut self,
    ) -> std::collections::vec_deque::Drain<'_, bytes::Bytes> {
        self.write_packets_to_send();
        self.outbox.drain(..)
    }
    // used by tests
    #[allow(dead_code)]
    pub fn has_packets_to_send(&self) -> bool {
        !self.outbox.is_empty()
    }

    #[allow(unused)]
    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    #[allow(unused)]
    pub fn counters(&self) -> &EndpointCounters {
        &self.counters
    }

    #[allow(unused)]
    pub fn sent_info(&self, sent_handle: SentHandle) -> Option<&SentData> {
        self.sent_buffer.get(sent_handle.0)
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

    fn next_packet_header(&mut self) -> PacketHeader {
        self.sequence = self.sequence.wrapping_add(1);
        let sequence = self.sequence;

        let (ack, ack_bits) = self.recv_buffer.ack_bits();

        let header = PacketHeader::new(sequence, ack, ack_bits);

        debug!(
            ">>> Sending packet seq:{} ack:{} ack_bits:{:#0b}",
            header.sequence(),
            header.ack(),
            header.ack_bits()
        );
        header
    }

    fn register_sent_packet(
        &mut self,
        sequence: u16,
        packet_len: usize,
    ) -> Result<(), ReliableError> {
        let send_size = packet_len + self.config.packet_header_size;
        let sent = SentData::new(self.time, send_size);
        self.sent_buffer.insert(sent, sequence)?;
        Ok(())
    }

    // when creating the messages, we want one big BytesMut?? with views into it, refcounted so
    // once no more messages are alive, it's cleaned up? then we can do a large contiguous allocation
    // for lots of small message buffers..
    // otherwise it's fragmenty af
    // it's almost an arena allocator cleared per frame, but some messages might not be sent until next frame,
    // and reliables need to stick around even longer..
    //
    //
    fn write_packets_to_send(&mut self) -> Result<(), ReliableError> {
        // info!("write packets.");
        let mut sent_something = false;

        let mut message_handles_in_packet = Vec::<MessageHandle>::new();
        let max_packet_size = self.config.max_packet_size;

        let mut packet: Option<BytesMut> = None;
        let mut remaining_space = max_packet_size;

        // definitely scope to optimise these nested loops..
        // hopefully never sending too many packets per tick though, so maybe fine.
        while self.channels.any_with_messages_to_send() {
            if packet.is_none() {
                packet = Some(BytesMut::with_capacity(max_packet_size));
                let header = self.next_packet_header();
                header.write(packet.as_mut().unwrap())?;
                remaining_space = max_packet_size - header.size();
            }

            // info!("any with msg to send");
            // for all channels with messages to send:
            'non_empty_channels: while let Some(channel) = self.channels.all_non_empty_mut().next()
            {
                let mut any_found = false;
                while let Some(msg) = channel.get_message_to_write_to_a_packet(remaining_space) {
                    any_found = true;
                    // trace!("* Writing {msg:?} to packet buffer..");
                    msg.write(packet.as_mut().unwrap())
                        .expect("writing to a buffer shouldn't fail");
                    message_handles_in_packet.push(MessageHandle {
                        id: msg.id(),
                        frag_index: msg.fragment().map(|f| f.index),
                        channel: channel.id(),
                    });
                    remaining_space = max_packet_size - packet.as_ref().unwrap().len();
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
            sent_something = true;
            // create final packet to send, leaving `packet` as None.
            let final_packet = packet.take().unwrap().freeze();
            self.send_packet(final_packet)?;
            // track which message ids are in which packet handle, so that when a packet handle
            // is acked, we can ack the corresponding message ids
            self.dispatcher.set_packet_message_handles(
                SentHandle(self.sequence),
                take(&mut message_handles_in_packet),
            );
        }

        // if no Messages to send, we'll still send an empty-payload packet, so that
        // acks are transmitted.
        // sending one empty packet per tick is fine.. right? what about uncapped headless server?
        if !sent_something {
            let mut empty_packet = BytesMut::with_capacity(max_packet_size);
            let header = self.next_packet_header();
            header.write(&mut empty_packet)?;
            self.send_packet(empty_packet.freeze())?;
        }
        Ok(())
    }

    /// "Sending" a packet involves writing a record into the sent buffer,
    /// incrementing the stats counters, and placing the packet into the outbox,
    /// so the consumer code can fetch and dispatch it via whatever means they like.
    fn send_packet(&mut self, packet: Bytes) -> Result<(), ReliableError> {
        let send_size = packet.len() + self.config.packet_header_size;
        self.sent_buffer
            .insert(SentData::new(self.time, send_size), self.sequence)?;
        self.outbox.push_back(packet);
        self.counters.packets_sent += 1;
        Ok(())
    }

    pub fn update(&mut self, dt: f64) {
        self.time += dt;
        // updating time for channels may result in reliable channels enqueuing messages
        // that need to be retransmitted.
        for channel in self.channels.all_mut() {
            channel.update(dt);
        }
    }

    /// Called by consumer with a packet they just read of the network.
    ///
    /// Will parse packet headers, deliver message acks to dispatcher, and
    /// parse out the messages for returning.
    ///
    ///  TODO make this take a &[u8] too?
    pub fn process_incoming_packet(&mut self, mut packet: Bytes) -> Result<(), ReliableError> {
        self.counters.packets_received += 1;
        let header: PacketHeader = PacketHeader::parse(&mut packet)?;
        debug!(
            "<<< Receiving packet seq:{} ack:{} ack_bits:{:#0b}",
            header.sequence(),
            header.ack(),
            header.ack_bits()
        );
        // if this packet sequence is out of range, reject as stale
        if !self.recv_buffer.check_sequence(header.sequence()) {
            log::warn!("Ignoring stale packet: {}", header.sequence());
            self.counters.packets_stale += 1;
            return Err(ReliableError::StalePacket);
        }
        // if this packet was already received, reject as duplicate
        if self.recv_buffer.exists(header.sequence()) {
            log::warn!("Ignoring duplicate packet: {}", header.sequence());
            self.counters.packets_duplicate += 1;
            return Err(ReliableError::DuplicatePacket);
        }
        self.recv_buffer.insert(
            RecvData::new(self.time, self.config.packet_header_size + packet.len()),
            header.sequence(),
        )?;
        // walk the ack bit field, and for any ack not already seen, we inform the dispatcher
        // so that it can ack all messages associated with that packet sequence number.
        let mut ack_bits = header.ack_bits();
        // create ack field for last 32 msgs
        for i in 0..32 {
            if ack_bits & 1 != 0 {
                let ack_sequence = header.ack().wrapping_sub(i);

                if let Some(sent_data) = self.sent_buffer.get_mut(ack_sequence) {
                    if !sent_data.acked {
                        // new ack!
                        self.counters.packets_acked += 1;
                        sent_data.acked = true;
                        // this allows the dispatcher to ack the messages that were sent in this packet
                        self.dispatcher
                            .acked_packet(&SentHandle(ack_sequence), &mut self.channels);
                        // update rtt calculations
                        let rtt: f32 = (self.time - sent_data.time) as f32 * 1000.0;
                        if (self.rtt == 0.0 && rtt > 0.0) || (self.rtt - rtt).abs() < 0.00001 {
                            self.rtt = rtt;
                        } else {
                            self.rtt =
                                self.rtt + ((rtt - self.rtt) * self.config.rtt_smoothing_factor);
                        }
                    }
                }
            }
            ack_bits >>= 1;
        }
        // Now extract all the Messages in the packet payload, and hand off to dispatcher
        while packet.remaining() > 0 {
            match Message::parse(&mut packet) {
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
        Ok(())
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
    fn duplicates_dropped() {
        init_logger();

        let mut server = Packeteer::default();
        let mut client = Packeteer::default();
        let payload = Bytes::from_static(b"hello");
        let msg_id = server.send_message(0, payload.as_ref());

        let to_send = server.drain_packets_to_send().collect::<Vec<_>>();
        assert_eq!(to_send.len(), 1);

        info!("Sending first copy of packet");
        assert!(client.process_incoming_packet(to_send[0].clone()).is_ok());

        let rec = client.drain_received_messages(0).next().unwrap();
        assert_eq!(rec.channel, 0);
        assert_eq!(rec.payload, payload);

        // send dupe
        info!("Sending second (dupe) copy of packet");
        match client.process_incoming_packet(to_send[0].clone()) {
            Err(ReliableError::DuplicatePacket) => {}
            e => {
                panic!("Should be dupe packet error, got: {:?}", e);
            }
        }
    }

    #[test]
    fn acks() {
        crate::test_utils::init_logger();

        let time = 100.0;
        let test_data = &[0x41; 24]; // "AAA..."

        let mut server = Packeteer::default();
        let mut client = Packeteer::default();

        let delta_time = 0.01;
        let mut server_sent = Vec::new();
        let mut client_sent = Vec::new();

        for _ in 0..200 {
            // Send test packets
            let handle1 = server.send_message(0, test_data);
            let handle2 = client.send_message(0, test_data);

            server_sent.push(handle1);
            client_sent.push(handle2);

            // forward packets to their endpoints
            server.drain_packets_to_send().for_each(|packet| {
                client.process_incoming_packet(packet);
                assert_eq!(
                    test_data,
                    client
                        .drain_received_messages(0)
                        .next()
                        .unwrap()
                        .payload
                        .as_ref()
                );
            });
            assert!(!server.has_packets_to_send());
            client.drain_packets_to_send().for_each(|packet| {
                server.process_incoming_packet(packet);
                assert_eq!(
                    test_data,
                    server
                        .drain_received_messages(0)
                        .next()
                        .unwrap()
                        .payload
                        .as_ref()
                );
            });
            assert!(!client.has_packets_to_send());

            server.update(delta_time);
            client.update(delta_time);
        }
    }

    const TEST_BUFFER_SIZE: usize = 256;

    #[test]
    fn ack_bits() {
        crate::test_utils::init_logger();

        #[allow(unused)]
        #[derive(Debug, Clone, Default)]
        struct TestData {
            sequence: u16,
        }

        let mut buffer = SequenceBuffer::<TestData>::with_capacity(TEST_BUFFER_SIZE);

        for i in 0..TEST_BUFFER_SIZE + 1 {
            buffer
                .insert(TestData { sequence: i as u16 }, i as u16)
                .unwrap();
        }

        let (ack, ack_bits) = buffer.ack_bits();
        println!("ack_bits = {ack_bits:#032b}");
        assert_eq!(ack, TEST_BUFFER_SIZE as u16);
        assert_eq!(ack_bits, 0xFFFFFFFF);

        ////

        // buffer.reset();
        buffer = SequenceBuffer::<TestData>::with_capacity(TEST_BUFFER_SIZE);

        for ack in [1, 5, 9, 11].iter() {
            buffer
                .insert(
                    TestData {
                        sequence: *ack as u16,
                    },
                    *ack as u16,
                )
                .unwrap();
        }

        let (ack, ack_bits) = buffer.ack_bits();
        let expected_ack_bits = 1 | (1 << (11 - 9)) | (1 << (11 - 5)) | (1 << (11 - 1));

        assert_eq!(ack, 11);

        println!("ack_bits = {ack_bits:#032b}");
        println!("expected = {expected_ack_bits:#032b}");
        // bits that should be set:
        assert_eq!(ack_bits, expected_ack_bits);
    }

    #[test]
    fn sequence_test() {
        crate::test_utils::init_logger();

        #[derive(Debug, Clone, Default)]
        struct TestData {
            sequence: u16,
        }

        let mut buffer = SequenceBuffer::<TestData>::with_capacity(TEST_BUFFER_SIZE);

        assert_eq!(buffer.capacity(), TEST_BUFFER_SIZE as usize);
        assert_eq!(buffer.sequence(), 0);

        for i in 0..TEST_BUFFER_SIZE {
            let r = buffer.get(i as u16);
            assert!(r.is_none());
        }

        for i in 0..TEST_BUFFER_SIZE * 4 {
            buffer
                .insert(TestData { sequence: i as u16 }, i as u16)
                .unwrap();
            assert_eq!(buffer.sequence(), i as u16);

            let r = buffer.get(i as u16);
            assert_eq!(r.unwrap().sequence, i as u16);
        }

        for i in 0..TEST_BUFFER_SIZE - 1 {
            let r = buffer.insert(TestData { sequence: i as u16 }, i as u16);
            assert!(r.is_err());
        }

        let mut index = TEST_BUFFER_SIZE * 4 - 1;
        for _ in 0..TEST_BUFFER_SIZE - 1 {
            let entry = buffer.get(index as u16);
            assert!(entry.is_some());
            let e = entry.unwrap();
            assert_eq!(e.sequence, index as u16);
            index -= 1;
        }
    }

    #[test]
    fn packet_header() {
        crate::test_utils::init_logger();

        let write_sequence = 10000;
        let write_ack = 100;
        let write_ack_bits = 123;

        let mut buffer = BytesMut::new();

        let write_packet = PacketHeader::new(write_sequence, write_ack, write_ack_bits);
        write_packet.write(&mut buffer).unwrap();

        let mut reader = Bytes::copy_from_slice(buffer.as_ref());
        let read_packet = PacketHeader::parse(&mut reader).unwrap();

        assert_eq!(write_packet.sequence(), read_packet.sequence());
        assert_eq!(write_packet.ack(), read_packet.ack());
        assert_eq!(write_packet.ack_bits(), read_packet.ack_bits());
    }

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
