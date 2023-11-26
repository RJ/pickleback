#![warn(missing_docs, clippy::missing_errors_doc, clippy::missing_panics_doc)]
#![doc = include_str!("../README.md")]

use cursor::{BufferLimitedWriter, CursorExtras};
///
use log::*;
use std::{
    collections::{HashMap, VecDeque},
    io::{Cursor, Write},
};
mod ack_header;
mod buffer_pool;
mod channel;
mod config;
mod cursor;
mod dispatcher;
mod error;
mod jitter_pipe;
mod message;
mod message_reassembler;
mod packet;
mod received_message;
mod sequence_buffer;
pub mod test_utils;
mod tracking;

use ack_header::*;
use buffer_pool::*;
use channel::*;
use config::*;
use dispatcher::*;
use error::*;
use jitter_pipe::*;
use message::*;
use message_reassembler::*;
use packet::*;
use received_message::*;
use sequence_buffer::*;
use tracking::*;

/// Easy importing of all the important bits
pub mod prelude {
    pub use super::config::PacketeerConfig;
    pub use super::error::{Backpressure, PacketeerError};
    pub use super::jitter_pipe::JitterPipeConfig;
    pub use super::message::MessageId;
    pub use super::received_message::ReceivedMessage;
    pub use super::tracking::PacketeerStats;
    pub use super::Packeteer;
}

/// Identifies a packet - contains sequence number
#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy, PartialOrd, Ord, Default)]
pub(crate) struct PacketId(pub(crate) u16);

/// An instance of Packeteer represents one of the two endpoints at either side of a link
pub struct Packeteer {
    time: f64,
    rtt: f32,
    config: PacketeerConfig,
    sequence_id: u16,
    newest_ack: Option<u16>,
    /// We know we've successfully acked up to this point, so when constructing the ack field in
    /// outbound packet headers, we don't need to bother acking anything lower than this id:
    ack_low_watermark: Option<PacketId>,
    dispatcher: MessageDispatcher,
    channels: ChannelList,
    sent_buffer: SequenceBuffer<SentMeta>,
    received_buffer: SequenceBuffer<ReceivedMeta>,
    stats: PacketeerStats,
    outbox: VecDeque<BufHandle>,
    pool: BufPool,
}

impl Default for Packeteer {
    fn default() -> Self {
        Self::new(PacketeerConfig::default(), 1.0)
    }
}

/// Represents one end of a datagram stream between two peers, one of which is the server.
///
impl Packeteer {
    /// Create new Packeteer instance.
    ///
    /// `time` should probably match your game loop time, which might be the number of seconds
    /// since the game started. You'll be updating this with a `dt` every tick, via `update()`.
    pub fn new(config: PacketeerConfig, time: f64) -> Self {
        let mut channels = ChannelList::default();
        channels.insert(Channel::from(UnreliableChannel::new(0, time)));
        channels.insert(Channel::from(ReliableChannel::new(1, time)));
        Self {
            time,
            rtt: 0.0,
            config: config.clone(),
            sequence_id: 0,
            newest_ack: None,
            ack_low_watermark: None,
            sent_buffer: SequenceBuffer::with_capacity(config.sent_packets_buffer_size),
            received_buffer: SequenceBuffer::with_capacity(config.received_packets_buffer_size),
            dispatcher: MessageDispatcher::new(&config),
            stats: PacketeerStats::default(),
            outbox: VecDeque::new(),
            channels,
            pool: BufPool::default(),
        }
    }

    /// Returns `PacketeerStats`, which tracks metrics on packet and message counts, etc.
    #[allow(unused)]
    pub fn stats(&self) -> &PacketeerStats {
        &self.stats
    }

    /// Draining iterator over packets in the outbox that we need to send over the network.
    ///
    /// Call this and send packets to the other Packeteer endpoint via a network transport.
    ///
    /// # Panics
    ///
    /// If unable to write all packets, for whatever reason (for development..)
    ///
    pub fn drain_packets_to_send(&mut self) -> std::collections::vec_deque::Drain<'_, BufHandle> {
        match self.write_packets_to_send() {
            Ok(()) => self.outbox.drain(..),
            Err(PacketeerError::Backpressure(bp)) => {
                // Might have written some packets, but not all.
                // still want to return the iterator.
                // TODO This should be a reportable non-fatal condition of some sort.
                warn!("[{}] Can't fully send this tick: {bp:?}", self.time);
                self.outbox.drain(..)
            }
            Err(e) => panic!("error writing packets to send: {e:?}"),
        }
    }

    /// Drains the list of received messages, which were parsed from received packets.
    pub fn drain_received_messages(&mut self, channel: u8) -> std::vec::Drain<'_, ReceivedMessage> {
        self.dispatcher.drain_received_messages(channel)
    }

    /// Drains the list of acked message ids.
    ///
    /// Once your `MessageId` is acked, it means we received a packet back from the remote endpoint
    /// saying that message ID was received.
    ///
    /// (In fact, acks happen at a packet level, not a message level – and then packet acks are
    /// translated onto message acks.)
    pub fn drain_message_acks(&mut self, channel: u8) -> std::vec::Drain<'_, MessageId> {
        self.dispatcher.drain_message_acks(channel)
    }

    /// Enqueue a message to be sent in the next available packet, via a channel.
    ///
    ///
    /// # Errors
    ///
    /// * PacketeerError::PayloadTooBig - if `message_payload` size exceeds config.max_message_size
    /// * PacketeerError::Backpressure(_) - can't send for some reason
    /// * PacketeerError::NoSuchChannel - you provided an invalid channel
    pub fn send_message(
        &mut self,
        channel: u8,
        message_payload: &[u8],
    ) -> Result<MessageId, PacketeerError> {
        if message_payload.len() > self.config.max_message_size {
            return Err(PacketeerError::PayloadTooBig);
        }
        // calling send_message doesn't generate packets or update the sent_unacked_packets count,
        // but perhaps last tick sent enough packets that this condition exists now, and if so, we
        // can reject sends here. Not sure if we should, or if it should be delegated to channels,
        // but for now it suits the tests to reject here:
        if self.sent_unacked_packets() >= MAX_UNACKED_PACKETS {
            warn!("Can't sent, too many pending");
            return Err(PacketeerError::Backpressure(Backpressure::TooManyPending));
        }
        self.stats.message_sends += 1;
        let Some(channel) = self.channels.get_mut(channel) else {
            return Err(PacketeerError::NoSuchChannel);
        };
        self.dispatcher
            .add_message_to_channel(&self.pool, channel, message_payload)
    }

    /// How many packets we've received that an ack hasn't been seen for yet.
    ///
    /// This is based on a full RTT of them (implicitly) acking our acks, and represents a lower
    /// bound used to construct the ackfield we include in packet headers.
    ///
    /// ie, used to decide what range of acks to send with every outbound packet.
    fn received_unacked_packets(&self) -> u16 {
        if let Some(lowest_ack) = self.ack_low_watermark {
            self.received_buffer.sequence().wrapping_sub(lowest_ack.0)
        } else {
            // nothing has been acked yet, so everything?
            self.received_buffer.sequence()
        }
    }

    /// How many packets have we sent that we haven't received acks for yet?
    pub fn sent_unacked_packets(&self) -> u16 {
        self.sent_buffer
            .sequence()
            .wrapping_sub(self.newest_ack.unwrap_or_default())
    }

    /// Creates a PacketHeader using the next packet sequence number, and an ack-field of recent acks.
    fn next_packet_header(&mut self) -> Result<PacketHeader, PacketeerError> {
        self.sequence_id = self.sequence_id.wrapping_add(1);
        let id = PacketId(self.sequence_id);
        let num_acks_required = self.received_unacked_packets();

        info!("Num acks required in packet id {id:?} = {num_acks_required}");

        if num_acks_required > MAX_UNACKED_PACKETS {
            return Err(PacketeerError::Backpressure(Backpressure::TooManyPending));
        }
        if num_acks_required == 0 {
            Ok(PacketHeader::new_no_acks(id)?)
        } else {
            let ack_iter = AckIter::with_minimum_length(&self.received_buffer, num_acks_required);
            let ret = PacketHeader::new(id, ack_iter, num_acks_required)?;
            info!("Packetheader: {ret:?}");
            Ok(ret)
        }
    }

    /// Calls `next_packet_header()` and writes it to the provided cursor, returning the bytes written
    fn write_packet_header(&mut self, cursor: &mut impl Write) -> Result<usize, PacketeerError> {
        let header = self.next_packet_header()?;
        debug!(
            ">>> Sending packet id:{:?} ack_id:{:?}",
            header.id(),
            header.ack_id(),
        );
        header.write(cursor)?;
        Ok(header.size())
    }

    /// For all the channels, coalesce any outbound messages they have into packets, with
    /// packet headers written. Place into outbox for eventual sending over the network.
    ///
    /// A "packet" is a buffer sized at the max_packet_size, which is approx. 1200 bytes.
    /// name: compose_packets?
    fn write_packets_to_send(&mut self) -> Result<(), PacketeerError> {
        if self.sent_unacked_packets() >= MAX_UNACKED_PACKETS {
            return Err(PacketeerError::Backpressure(Backpressure::TooManyPending));
        }

        let mut sent_something = false;
        let mut message_handles_in_packet = Vec::new();
        let max_packet_size = self.config.max_packet_size;
        let mut packet: Option<BufHandle> = None;
        // BufferLimitedWriter (which impls Write) wraps up `Cursor<&mut Vec<u8>>>` but limits how
        // many bytes can be written, and provides a .remaining() fn to query same.
        let mut writer: Option<BufferLimitedWriter> = None;

        while self.channels.any_with_messages_to_send() {
            if writer.is_none() {
                // before we allocate a new packet, ensure we aren't going to break the ack system
                // by having too many unacked packets in-flight.
                if self.sent_unacked_packets() >= MAX_UNACKED_PACKETS {
                    return Err(PacketeerError::Backpressure(Backpressure::TooManyPending));
                }
                packet = Some(self.pool.get_buffer(max_packet_size));
                let cur = Cursor::new(packet.as_mut().unwrap().as_mut());
                writer = Some(BufferLimitedWriter::new(cur, max_packet_size));
                self.write_packet_header(writer.as_mut().unwrap())?;
            }

            while let Some(channel) = self.channels.all_non_empty_mut().next() {
                match Self::write_channel_messages_to_packet(
                    channel,
                    writer.as_mut().unwrap(),
                    &mut message_handles_in_packet,
                )? {
                    0 => break,
                    num_written => {
                        self.stats.messages_sent += num_written as u64;
                    }
                }

                if writer.as_ref().unwrap().remaining() < 3 {
                    break;
                }
            }

            if writer.as_ref().unwrap().remaining() != max_packet_size {
                sent_something = true;
                writer = None;
                self.send_packet(packet.take().unwrap())?;
                self.dispatcher.set_packet_message_handles(
                    PacketId(self.sequence_id),
                    std::mem::take(&mut message_handles_in_packet),
                )?;
            }
        }

        if !sent_something {
            self.send_empty_packet()?;
        }
        Ok(())
    }

    /// Writes messages from the channel into the packet cursor, until there's no space left,
    /// or the channel runs out of messages to send.
    ///
    /// Returns number of messages written.
    fn write_channel_messages_to_packet(
        channel: &mut Channel,
        cursor: &mut BufferLimitedWriter,
        message_handles: &mut Vec<MessageHandle>,
    ) -> Result<usize, PacketeerError> {
        let mut num_written = 0;
        while let Some(msg) = channel.get_message_to_write_to_a_packet(cursor.remaining()) {
            num_written += 1;
            cursor.write_all(msg.as_slice())?;
            message_handles.push(MessageHandle {
                id: msg.id(),
                frag_index: msg.fragment().map(|f| f.index),
                channel: channel.id(),
            });
            if cursor.remaining() < 3 {
                break;
            }
        }
        Ok(num_written)
    }

    /// Sends a packet containing zero messages.
    ///
    /// The packet  header contains the ack field, so it can be useful to send an empty packet
    /// just to send acks. We do this if there are no messages to send this tick.
    fn send_empty_packet(&mut self) -> Result<(), PacketeerError> {
        if self.sent_unacked_packets() >= MAX_UNACKED_PACKETS {
            return Err(PacketeerError::Backpressure(Backpressure::TooManyPending));
        }
        let mut packet = Some(self.pool.get_buffer(1300));
        let mut cursor = Cursor::new(packet.as_mut().unwrap().as_mut());
        self.write_packet_header(&mut cursor)?;
        self.send_packet(packet.take().unwrap())
    }

    /// "Send" a fully formed packet, by writing a record into the sent buffer,
    /// incrementing the stats counters, and placing the packet into the outbox
    ///
    /// The BufHandle is for a packet with headers written, usually in `write_packets_to_send()`.
    ///
    /// The consumer code should fetch and dispatch it via whatever means they like.
    fn send_packet(&mut self, packet: BufHandle) -> Result<(), PacketeerError> {
        let send_size = packet.len() + self.config.packet_header_size;
        // received_buffer.sequence() is the most recent packet ack we're acknowledging reciept of in the
        // header of this packet we're about to send.
        // We include it in the SentData so, once OUR packet gets acked, we have a low-watermark
        // telling us we don't need to bother acking anything prior to this id in future packets.
        let acking_up_to = self.received_buffer.sequence();
        self.sent_buffer.insert(
            SentMeta::new(self.time, send_size, PacketId(acking_up_to)),
            self.sequence_id,
        )?;
        self.outbox.push_back(packet);
        self.stats.packets_sent += 1;
        info!(
            "Sent packet {:?}, acks_in_flight: {}",
            self.sequence_id,
            self.received_unacked_packets()
        );
        Ok(())
    }

    /// Advance the time by `dt` seconds.
    ///
    /// When ticking in your game loop, you must advance the time within Packeteer too, by passing
    /// in the delta time since you last called update.
    ///
    /// This is so it knows when to schedule re-sends of data, and can calculate rtt correctly.
    pub fn update(&mut self, dt: f64) {
        self.time += dt;
        // updating time for channels may result in reliable channels enqueuing messages
        // that need to be retransmitted.
        for channel in self.channels.all_mut() {
            channel.update(dt);
        }
    }

    /// Parse a received packet, reading the header and all messages contained within.
    ///
    /// Called by consumer with a packet they just read from the network.
    /// Extracted messages are delivered to channels, acks are extracted for later consumption.
    ///
    /// # Errors
    ///
    /// * Can return a PacketeerError::Io if parsing packet headers or messages fails.
    /// * Can return PacketeerError::StalePacket
    /// /// * Can return PacketeerError::DuplicatePacket
    pub fn process_incoming_packet(&mut self, packet: &[u8]) -> Result<(), PacketeerError> {
        self.stats.packets_received += 1;
        let mut reader = Cursor::new(packet);
        let header: PacketHeader = PacketHeader::parse(&mut reader)?;
        log::trace!(
            "<<< Receiving packet id:{:?} ack_id:{:?}",
            header.id(),
            header.ack_id(),
        );
        // if this packet sequence is out of range, reject as stale
        if !self.received_buffer.check_sequence(header.id().0) {
            log::debug!("Ignoring stale packet: {}", header.id().0);
            self.stats.packets_stale += 1;
            return Err(PacketeerError::StalePacket);
        }
        // if this packet was already received, reject as duplicate
        if self.received_buffer.exists(header.id().0) {
            log::debug!("Ignoring duplicate packet: {:?}", header.id());
            self.stats.packets_duplicate += 1;
            return Err(PacketeerError::DuplicatePacket);
        }
        self.received_buffer.insert(
            ReceivedMeta::new(self.time, self.config.packet_header_size + packet.len()),
            header.id().0,
        )?;
        self.process_packet_acks_and_rtt(&header);
        self.process_packet_messages(&mut reader)?;

        Ok(())
    }

    /// Parses Messages from packet payload and delivers them to the dispatcher
    fn process_packet_messages(
        &mut self,
        reader: &mut Cursor<&[u8]>,
    ) -> Result<(), PacketeerError> {
        while reader.remaining() > 0 {
            // as long as there are bytes left to read, we should only find whole messages
            let msg = Message::parse(&self.pool, reader)?;
            self.stats.messages_received += 1;
            info!("Parsed from packet: {msg:?}");
            if let Some(channel) = self.channels.get_mut(msg.channel()) {
                if channel.accepts_message(&msg) {
                    self.dispatcher.process_received_message(msg);
                } else {
                    trace!("Channel rejects message id {msg:?}");
                }
            }
        }
        Ok(())
    }

    /// Parses ack bitfield and checks for unseen acks, reports them to the dispatcher so it can
    /// ack the message ids associated with the packet sequence nunmber.
    /// Updates rtt calculations.
    fn process_packet_acks_and_rtt(&mut self, header: &PacketHeader) {
        let Some(ack_iter) = header.acks() else {
            return;
        };
        for (ack_sequence, is_acked) in ack_iter {
            if !is_acked {
                continue;
            }
            if let Some(sent_data) = self.sent_buffer.get_mut(ack_sequence) {
                if !sent_data.acked {
                    // Tracking newest ack we've seen, so we know how many packets are unacked
                    // or still in flight.
                    if let Some(newest_ack) = self.newest_ack.as_mut() {
                        if SequenceBuffer::<u16>::sequence_greater_than(ack_sequence, *newest_ack) {
                            *newest_ack = ack_sequence;
                        }
                    } else {
                        self.newest_ack = Some(ack_sequence);
                    }
                    // SentData stores which of their packets we acked up to in our packet,
                    // so we can now potentially move our ack low-watermark up.
                    // ie, they have effectively acked our acks up to this point.
                    if let Some(lowest_ack) = self.ack_low_watermark {
                        if SequenceBuffer::<u16>::sequence_greater_than(
                            sent_data.acked_up_to.0,
                            lowest_ack.0,
                        ) {
                            self.ack_low_watermark = Some(sent_data.acked_up_to);
                        }
                    } else {
                        self.ack_low_watermark = Some(sent_data.acked_up_to);
                    }
                    //
                    self.stats.packets_acked += 1;
                    sent_data.acked = true;
                    // this allows the dispatcher to ack the messages that were sent in this packet
                    self.dispatcher
                        .acked_packet(PacketId(ack_sequence), &mut self.channels);
                    // update rtt calculations
                    let rtt: f32 = (self.time - sent_data.time) as f32 * 1000.0;
                    if (self.rtt == 0.0 && rtt > 0.0) || (self.rtt - rtt).abs() < 0.00001 {
                        self.rtt = rtt;
                    } else {
                        self.rtt = self.rtt + ((rtt - self.rtt) * self.config.rtt_smoothing_factor);
                    }
                }
            }
        }
    }

    // used by tests
    #[allow(dead_code)]
    fn channels_mut(&mut self) -> &mut ChannelList {
        &mut self.channels
    }

    /// Are there packets sitting in the outbox?
    ///
    /// Only used by tests
    #[allow(dead_code)]
    fn has_packets_to_send(&self) -> bool {
        !self.outbox.is_empty()
    }

    /// Returns the config you passed into `new()`
    #[allow(unused)]
    pub fn config(&self) -> &PacketeerConfig {
        &self.config
    }

    /// Access to the SentData for a specific packet sequence number.
    ///
    /// Used in tests only.
    #[allow(unused)]
    fn sent_info(&self, sent_handle: PacketId) -> Option<&SentMeta> {
        self.sent_buffer.get(sent_handle.0)
    }
}

#[cfg(test)]
mod tests {
    use crate::jitter_pipe::JitterPipeConfig;

    use super::*;
    use test_utils::*;

    #[test]
    fn drop_duplicate_packets() {
        init_logger();

        let mut server = Packeteer::default();
        let mut client = Packeteer::default();
        let payload = b"hello";
        let _msg_id = server.send_message(0, payload);

        let to_send = server.drain_packets_to_send().collect::<Vec<_>>();
        assert_eq!(to_send.len(), 1);

        info!("Sending first copy of packet");
        assert!(client.process_incoming_packet(to_send[0].as_ref()).is_ok());

        let rec = client.drain_received_messages(0).next().unwrap();
        assert_eq!(rec.channel(), 0);

        assert_eq!(rec.payload_to_owned().as_slice(), payload.as_ref());

        // send dupe
        info!("Sending second (dupe) copy of packet");
        match client.process_incoming_packet(to_send[0].as_ref()) {
            Err(PacketeerError::DuplicatePacket) => {}
            e => {
                panic!("Should be dupe packet error, got: {:?}", e);
            }
        }
    }

    #[test]
    fn many_packets_in_flight() {
        crate::test_utils::init_logger();
        let mut server = Packeteer::default();
        let mut client = Packeteer::default();
        let msg = &[0x42_u8; 1024];
        let mut sent_ids = Vec::new();
        // sending this many full-packet-sized messages is the max we can send before acks arrive
        for _ in 0..MAX_UNACKED_PACKETS {
            sent_ids.push(server.send_message(0, msg).unwrap());
        }
        // update server so it sends packets.
        // this will make server realise it has MAX_UNACKED_PACKETS in flight, and shouldn't be able
        // to send anything else until it sees some acks.
        server.update(1.0);
        info!("Server sending to client..");
        server.drain_packets_to_send().for_each(|packet| {
            client.process_incoming_packet(packet.as_ref()).unwrap();
        });
        info!("sent_unacked is now: {}", server.sent_unacked_packets());
        info!("Send attempt for MAX_UNACKED_PACKETS+1");
        match server.send_message(0, msg) {
            Err(PacketeerError::Backpressure(Backpressure::TooManyPending)) => {}
            other => panic!("Expecting backpressure, got {other:?}"),
        }

        client.update(1.0);
        // exchange packets
        info!(
            "Client sending to server.. sent_unacked is {}",
            client.sent_unacked_packets()
        );
        client.drain_packets_to_send().for_each(|packet| {
            server.process_incoming_packet(packet.as_ref()).unwrap();
        });

        assert_eq!(sent_ids.len(), client.drain_received_messages(0).len());
        assert_eq!(sent_ids.len(), server.drain_message_acks(0).len());
    }

    #[test]
    fn acks() {
        crate::test_utils::init_logger();

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
                client.process_incoming_packet(packet.as_ref()).unwrap();
                assert_eq!(
                    test_data,
                    client
                        .drain_received_messages(0)
                        .next()
                        .unwrap()
                        .payload_to_owned()
                        .as_slice()
                );
            });
            assert!(!server.has_packets_to_send());
            client.drain_packets_to_send().for_each(|packet| {
                server.process_incoming_packet(packet.as_ref()).unwrap();
                assert_eq!(
                    test_data,
                    server
                        .drain_received_messages(0)
                        .next()
                        .unwrap()
                        .payload_to_owned()
                        .as_slice()
                );
            });
            assert!(!client.has_packets_to_send());

            server.update(delta_time);
            client.update(delta_time);
        }
    }

    const TEST_BUFFER_SIZE: usize = 256;

    #[test]
    fn sequence_test() {
        crate::test_utils::init_logger();

        #[derive(Debug, Clone, Default)]
        struct TestData {
            sequence: u16,
        }

        let mut buffer = SequenceBuffer::<TestData>::with_capacity(TEST_BUFFER_SIZE);

        assert_eq!(buffer.capacity(), TEST_BUFFER_SIZE);
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

        let write_id = PacketId(10000);
        let ack_iter = [(100, true)].into_iter();

        let mut buffer = Vec::new();
        let mut cur = Cursor::new(&mut buffer);
        let write_packet = PacketHeader::new(write_id, ack_iter, 1).unwrap();
        write_packet.write(&mut cur).unwrap();

        let mut reader = Cursor::new(buffer.as_ref());
        let read_packet = PacketHeader::parse(&mut reader).unwrap();

        assert_eq!(write_packet.id(), read_packet.id());
        assert_eq!(write_packet.ack_id(), read_packet.ack_id());
        assert_eq!(
            write_packet.acks().unwrap().collect::<Vec<_>>(),
            read_packet.acks().unwrap().collect::<Vec<_>>()
        );
    }

    #[test]
    fn small_unfrag_messages() {
        crate::test_utils::init_logger();
        let channel = 0;
        let mut harness = TestHarness::new(JitterPipeConfig::disabled());

        let msg1 = b"Hello";
        let msg2 = b"world";
        let msg3 = b"!";
        let msg4 = b"";
        let id1 = harness.server.send_message(channel, msg1).unwrap();
        let id2 = harness.server.send_message(channel, msg2).unwrap();
        let id3 = harness.server.send_message(channel, msg3).unwrap();
        let id4 = harness.server.send_message(channel, msg4).unwrap();

        harness.advance(0.1);

        let received_messages = harness
            .client
            .drain_received_messages(channel)
            .collect::<Vec<_>>();
        assert_eq!(received_messages[0].payload_to_owned().as_slice(), msg1);
        assert_eq!(received_messages[1].payload_to_owned().as_slice(), msg2);
        assert_eq!(received_messages[2].payload_to_owned().as_slice(), msg3);
        assert_eq!(received_messages[3].payload_to_owned().as_slice(), msg4);

        assert_eq!(
            vec![id1, id2, id3, id4],
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

        let msg_id = harness.server.send_message(channel, msg.as_ref()).unwrap();

        harness.advance(0.1);

        let received_messages = harness
            .client
            .drain_received_messages(channel)
            .collect::<Vec<_>>();
        assert_eq!(received_messages.len(), 1);
        assert_eq!(received_messages[0].payload_to_owned().as_slice(), msg);

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
        let pool = BufPool::empty();
        let channel = 0;
        let mut harness = TestHarness::new(JitterPipeConfig::disabled());
        let payload = b"hello";
        harness
            .server
            .channels_mut()
            .get_mut(channel)
            .unwrap()
            .enqueue_message(&pool, MessageId(123), payload, Fragmented::No);
        harness
            .server
            .channels_mut()
            .get_mut(channel)
            .unwrap()
            .enqueue_message(&pool, MessageId(123), payload, Fragmented::No);
        harness.advance(1.);
        assert_eq!(harness.client.drain_received_messages(channel).len(), 1);
    }

    // test what happens in a reliable channel when one of the fragments isn't delivered.
    // should resend after a suitable amount of time.
    #[test]
    fn retransmission() {
        let channel = 1;
        let mut harness = TestHarness::new(JitterPipeConfig::disabled());
        // big enough to require 2 packets
        let payload = random_payload(1800);
        let id = harness
            .server
            .send_message(channel, payload.as_ref())
            .unwrap();
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
}
