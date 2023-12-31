use std::ops::DerefMut;

use crate::*;

/// An instance of Pickleback represents one of the two endpoints at either side of a link
pub struct Pickleback {
    time: f64,
    rtt: f32,
    packet_loss: f32,
    config: PicklebackConfig,
    sequence_id: u16,
    newest_ack: Option<u16>,
    /// We know we've successfully acked up to this point, so when constructing the ack field in
    /// outbound packet headers, we don't need to bother acking anything lower than this id:
    ack_low_watermark: Option<PacketId>,
    /// as above but for packets worth acking. ignores keepalives and empty messages.
    ack_low_watermark_non_empty: Option<PacketId>,
    dispatcher: MessageDispatcher,
    channels: ChannelList,
    sent_buffer: SequenceBuffer<SentMeta>,
    received_buffer: SequenceBuffer<ReceivedMeta>,
    stats: PicklebackStats,
    outbox: VecDeque<PooledBuffer>,
    pool: BufPool,
    xor_salt: Option<u64>,
}

impl Default for Pickleback {
    fn default() -> Self {
        Self::new(PicklebackConfig::default(), 1.0)
    }
}

/// Represents one end of a datagram stream between two peers, one of which is the server.
///
impl Pickleback {
    /// Create new Pickleback instance.
    ///
    /// `time` should probably match your game loop time, which might be the number of seconds
    /// since the game started. You'll be updating this with a `dt` every tick, via `update()`.
    pub fn new(config: PicklebackConfig, time: f64) -> Self {
        let mut channels = ChannelList::default();
        channels.insert(Channel::from(UnreliableChannel::new(0, time)));
        channels.insert(Channel::from(ReliableChannel::new(1, time)));
        let pool = if let Some(buffer_pools_config) = &config.buffer_pools_config {
            BufPool::new(buffer_pools_config.clone())
        } else {
            BufPool::default()
        };
        Self {
            time,
            rtt: 0.0,
            packet_loss: 0.0,
            config: config.clone(),
            sequence_id: 0,
            newest_ack: None,
            ack_low_watermark: None,
            ack_low_watermark_non_empty: None,
            sent_buffer: SequenceBuffer::with_capacity(config.sent_packets_buffer_size),
            received_buffer: SequenceBuffer::with_capacity(config.received_packets_buffer_size),
            dispatcher: MessageDispatcher::new(&config),
            stats: PicklebackStats::default(),
            outbox: VecDeque::new(),
            channels,
            pool,
            xor_salt: None,
        }
    }

    /// reference to buffer pool. used for testing.
    pub fn pool(&self) -> &BufPool {
        &self.pool
    }

    /*
    pub(crate) fn reset(&mut self) {
        self.rtt = 0.0;
        self.packet_loss = 0.0;
        self.sequence_id = 0;
        self.newest_ack = None;
        self.ack_low_watermark = None;
        self.stats = PicklebackStats::default();
        self.outbox.clear();
        self.xor_salt = None;
        self.sent_buffer.reset();
        self.received_buffer.reset();
        self.dispatcher = MessageDispatcher::new(&self.config);
    }
    */

    pub(crate) fn set_xor_salt(&mut self, xor_salt: Option<u64>) {
        self.xor_salt = xor_salt;
    }

    /// Returns `PicklebackStats`, which tracks metrics on packet and message counts, etc.
    #[allow(unused)]
    pub fn stats(&self) -> &PicklebackStats {
        &self.stats
    }

    /// Round-trip-time estimation in seconds.
    #[allow(unused)]
    pub fn rtt(&self) -> f32 {
        self.rtt
    }

    /// Packet loss estimation percent.
    #[allow(unused)]
    pub fn packet_loss(&self) -> f32 {
        self.packet_loss
    }

    /// The current time, as advanced by calling `update()`.
    pub fn time(&self) -> f64 {
        self.time
    }

    /// Consumes internal buffer of pending outbound packets, calling `send_fn` on each one.
    ///
    /// Call this and send packets to the other Pickleback endpoint via the network.
    ///
    /// # Panics
    ///
    /// If unable to write all packets, for whatever reason except backpressure.
    ///
    pub fn visit_packets_to_send(&mut self, mut send_fn: impl FnMut(&[u8])) -> usize {
        let mut num_sent = 0;
        match self.compose_packets() {
            Ok(_) | Err(PicklebackError::Backpressure(_)) => {
                // even if backpressure, *some* packets may have been written to the outbox
                for packet in self.outbox.drain(..) {
                    send_fn(packet.as_ref());
                    self.pool.return_buffer(packet);
                    num_sent += 1;
                }
            }
            Err(e) => {
                panic!("Error writing packtes to send {e:?}");
            }
        }
        num_sent
    }

    /// Drain packets from the outbound queue into a vec, instead of using the visitor pattern
    /// via `visit_packets_to_send()` - this is just for tests.
    pub fn collect_packets_to_send_inefficiently(&mut self) -> Vec<Vec<u8>> {
        let mut ret = Vec::new();
        self.visit_packets_to_send(|s| ret.push(s.to_vec()));
        ret
    }

    /// How many packets are waiting in the outbound send queue.
    ///
    /// (You consume these using `visit_packets_to_send()`)
    pub fn num_packets_to_send(&self) -> usize {
        self.outbox.len()
    }

    /// Gives an iterator of received messages, which were parsed from received packets.
    ///
    /// Once iterator drops, pooled buffers within messages are returned to our pool.
    pub fn drain_received_messages(&mut self, channel: u8) -> ReceivedMessagesContainer {
        let messages = self.dispatcher.take_received_messages(channel);
        ReceivedMessagesContainer::new(messages, &mut self.pool)
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
    /// * PicklebackError::PayloadTooBig - if `message_payload` size exceeds config.max_message_size
    /// * PicklebackError::Backpressure(_) - can't send for some reason
    /// * PicklebackError::NoSuchChannel - you provided an invalid channel
    pub fn send_message(
        &mut self,
        channel: u8,
        message_payload: &[u8],
    ) -> Result<MessageId, PicklebackError> {
        if message_payload.len() > self.config.max_message_size {
            return Err(PicklebackError::PayloadTooBig);
        }
        // calling send_message doesn't generate packets or update the sent_unacked_packets count,
        // but perhaps last tick sent enough packets that this condition exists now, and if so, we
        // can reject sends here. Not sure if we should, or if it should be delegated to channels,
        // but for now it suits the tests to reject here:
        if self.sent_unacked_packets() >= MAX_UNACKED_PACKETS {
            warn!("Can't sent, too many pending");
            return Err(PicklebackError::Backpressure(Backpressure::TooManyPending));
        }
        self.stats.message_sends += 1;
        let Some(channel) = self.channels.get_mut(channel) else {
            return Err(PicklebackError::NoSuchChannel);
        };
        self.dispatcher
            .add_message_to_channel(&mut self.pool, channel, message_payload)
    }

    /// How many packets we've received that an ack hasn't been seen for yet.
    ///
    /// This is based on a full RTT of them (implicitly) acking our acks, and represents a lower
    /// bound used to construct the ackfield we include in packet headers.
    ///
    /// ie, used to decide what range of acks to send with every outbound packet.
    pub(crate) fn received_unacked_packets(&self) -> u16 {
        if let Some(lowest_ack) = self.ack_low_watermark {
            self.received_buffer.sequence().wrapping_sub(lowest_ack.0)
        } else {
            // nothing has been acked yet, so everything?
            self.received_buffer.sequence()
        }
    }

    /// How many packets we've received that an ack hasn't been seen for yet, but ignoring
    /// empty messages packets and keep alives. This tracks the number of unseen-ack packets
    /// that are actually worth sending another packet in order to transmit acks for.
    pub(crate) fn received_unacked_non_empty_packets(&self) -> u16 {
        if let Some(lowest_ack) = self.ack_low_watermark_non_empty {
            if !SequenceBuffer::<u16>::sequence_less_than(
                self.received_buffer.sequence(),
                lowest_ack.0,
            ) {
                0
            } else {
                self.received_buffer.sequence().wrapping_sub(lowest_ack.0)
            }
        } else {
            // nothing has been acked yet, so everything?
            0
            // self.received_buffer.sequence()
        }
    }

    /// How many packets have we sent that we haven't received acks for yet?
    pub fn sent_unacked_packets(&self) -> u16 {
        self.sent_buffer
            .sequence()
            .wrapping_sub(self.newest_ack.unwrap_or_default())
    }

    /// Creates a PacketHeader using the next packet sequence number, and an ack-field of recent acks.
    pub(crate) fn next_packet_header(
        &mut self,
        packet_type: PacketType,
    ) -> Result<ProtocolPacketHeader, PicklebackError> {
        self.sequence_id = self.sequence_id.wrapping_add(1);
        let id = PacketId(self.sequence_id);
        let num_acks_required = self.received_unacked_packets();
        // info!("Num acks required in packet id {id:?} = {num_acks_required}");
        if num_acks_required > MAX_UNACKED_PACKETS {
            warn!("Not composing packet, backpressure.");
            return Err(PicklebackError::Backpressure(Backpressure::TooManyPending));
        }

        let ack_iter = AckIter::with_minimum_length(&self.received_buffer, num_acks_required);
        let ph = ProtocolPacketHeader::new(id, ack_iter, num_acks_required, packet_type)?;
        Ok(ph)
    }

    /// Calls `next_packet_header()` and writes it to the provided cursor, returning the bytes written
    fn write_packet_header(&mut self, cursor: &mut impl Write) -> Result<usize, PicklebackError> {
        let header = self.next_packet_header(PacketType::Messages)?;
        debug!(
            ">>> Sending packet id:{:?} ack_id:{:?}",
            header.id(),
            header.ack_id(),
        );
        header.write(cursor)?;
        cursor.write_u64::<NetworkEndian>(
            self.xor_salt
                .expect("expected xor_salt when writing packet header for messages"),
        )?;
        Ok(header.size() + 8)
    }

    /// For all the channels, coalesce any outbound messages they have into packets, with
    /// packet headers written. Place into outbox for eventual sending over the network.
    ///
    /// A "packet" is a buffer sized at the max_packet_size, which is approx. 1200 bytes.
    /// name: compose_packets?
    fn compose_packets(&mut self) -> Result<(), PicklebackError> {
        if self.sent_unacked_packets() >= MAX_UNACKED_PACKETS {
            log::warn!("TooManyPending - backpressure");
            return Err(PicklebackError::Backpressure(Backpressure::TooManyPending));
        }

        let mut sent_something = false;
        let mut message_handles_in_packet = Vec::new();
        let max_packet_size = self.config.max_packet_size;
        let mut packet: Option<PooledBuffer> = None;
        // BufferLimitedWriter (which impls Write) wraps up `Cursor<&mut Vec<u8>>>` but limits how
        // many bytes can be written, and provides a .remaining() fn to query same.
        let mut writer: Option<BufferLimitedWriter> = None;

        while self.channels.any_with_messages_to_send() {
            if writer.is_none() {
                // before we allocate a new packet, ensure we aren't going to break the ack system
                // by having too many unacked packets in-flight.
                if self.sent_unacked_packets() >= MAX_UNACKED_PACKETS {
                    return Err(PicklebackError::Backpressure(Backpressure::TooManyPending));
                }
                // this packet is returned to the buffer after `visit_packets_to_send` sees it.
                packet = Some(self.pool.get_buffer(max_packet_size));
                let cur = Cursor::new(packet.as_mut().unwrap().deref_mut());
                writer = Some(BufferLimitedWriter::new(cur, max_packet_size));
                self.write_packet_header(writer.as_mut().unwrap())?;
            }

            while let Some(channel) = self.channels.all_non_empty_mut().next() {
                match Self::write_channel_messages_to_packet(
                    channel,
                    writer.as_mut().unwrap(),
                    &mut message_handles_in_packet,
                    &mut self.pool,
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
                self.send_raw_packet(packet.take().unwrap())?;
                self.dispatcher.set_packet_message_handles(
                    PacketId(self.sequence_id),
                    std::mem::take(&mut message_handles_in_packet),
                )?;
            }
        }

        // we should force send if still have ACKs to transmit that the remote hasn't acknowledged yet
        // TODO we mustn't care about acks on empty message or keepalive packets, otherwise
        //      we will always have something that needs acking..
        if !sent_something
            && self.received_unacked_non_empty_packets() > 0
            && self.xor_salt.is_some()
        {
            // log::warn!("Sending empty because received unacked non-empty..");
            // self.send_empty_packet()?;
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
        pool: &mut BufPool,
    ) -> Result<usize, PicklebackError> {
        let mut num_written = 0;
        while let Some(mut msg) = channel.get_message_to_write_to_a_packet(cursor.remaining()) {
            num_written += 1;
            cursor.write_all(msg.as_slice())?;
            message_handles.push(MessageHandle {
                id: msg.id(),
                frag_index: msg.fragment().map(|f| f.index),
                channel: channel.id(),
            });
            pool.return_buffer(msg.take_buffer());
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
    fn send_empty_packet(&mut self) -> Result<(), PicklebackError> {
        if self.sent_unacked_packets() >= MAX_UNACKED_PACKETS {
            return Err(PicklebackError::Backpressure(Backpressure::TooManyPending));
        }
        // this buffer is returned to the pool after the visit_packets_to_send sees it
        let mut packet = Some(self.pool.get_buffer(1300));
        let mut cursor = Cursor::new(packet.as_mut().unwrap().deref_mut());
        self.write_packet_header(&mut cursor)?;
        self.send_raw_packet(packet.take().unwrap())
    }

    /// Write a packet to a newly allocated buffer and send it.
    pub(crate) fn send_packet(&mut self, packet: ProtocolPacket) -> Result<(), PicklebackError> {
        // write_packet gets a pooled buffer, which is returned once visited for sending
        log::info!(">>> {:?}", packet);
        let buf = write_packet(&mut self.pool, &self.config, packet)?;
        self.send_raw_packet(buf)
    }

    // pub(crate) fn receive_packet(&mut self, packet: &[u8]) -> Result<ProtocolPacket, PicklebackError> {
    //     let
    // }

    /// "Send" a fully written packet (headers & payload), by writing a record into the sent buffer,
    /// incrementing the stats counters, and placing the packet into the outbox
    ///
    /// The BufHandle is for a packet with headers written, usually in `write_packets_to_send()`.
    ///
    /// The consumer code should fetch and dispatch it via whatever means they like.
    fn send_raw_packet(&mut self, packet: PooledBuffer) -> Result<(), PicklebackError> {
        let send_size = packet.len() + self.config.packet_header_size;
        // received_buffer.sequence() is the most recent packet ack we're acknowledging reciept of in the
        // header of this packet we're about to send.
        // We include it in the SentData so, once OUR packet gets acked, we have a low-watermark
        // telling us we don't need to bother acking anything prior to this id in future packets.
        let acking_up_to = self.received_buffer.sequence();
        self.sent_buffer.insert(
            self.sequence_id,
            SentMeta::new(self.time, send_size, PacketId(acking_up_to)),
        )?;
        self.outbox.push_back(packet);
        self.stats.packets_sent += 1;
        // info!(
        //     "Sent packet {:?}, acks_in_flight: {} non_empty_acks_in_flight: {}",
        //     self.sequence_id,
        //     self.received_unacked_packets(),
        //     self.received_unacked_non_empty_packets()
        // );
        Ok(())
    }

    /// Advance the time by `dt` seconds.
    ///
    /// When ticking in your game loop, you must advance the time within Pickleback too, by passing
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

    /// Process acks, update metadata tracking, recalcualte RTT etc.
    ///
    /// For PacketType::Messages this reads the remainder of the Cursor to parse Messages.
    ///
    /// Called by consumer with a packet they just read from the network.
    /// Extracted messages are delivered to channels, acks are extracted for later consumption.
    ///
    /// # Errors
    ///
    /// * Can return a PicklebackError::Io if parsing packet headers or messages fails.
    /// * Can return PicklebackError::StalePacket
    /// * Can return PicklebackError::DuplicatePacket
    pub(crate) fn process_incoming_packet(
        &mut self,
        header: &ProtocolPacketHeader,
        payload_reader: &mut Cursor<&[u8]>,
    ) -> Result<(), PicklebackError> {
        self.stats.packets_received += 1;
        log::trace!("<<< {header:?}");
        // if this packet sequence is out of buffer range, reject as stale
        if !self.received_buffer.check_sequence(header.id().0) {
            log::debug!("Ignoring stale packet: {}", header.id().0);
            self.stats.packets_stale += 1;
            return Err(PicklebackError::StalePacket);
        }

        // TODO we can only reject older sequence numbers for unreliable unfragmented messages atm
        // perhaps we need to delegate some checks to channels by passing the packetheader in?
        if !self.received_buffer.check_newer_than_current(header.id().0) {
            // log::debug!("Ignoring stale packet: {}", header.id().0);
            // self.stats.packets_stale += 1;
            // return Err(PicklebackError::StalePacket);
        }

        // if this packet was already received, reject as duplicate
        if self.received_buffer.exists(header.id().0) {
            log::debug!("Ignoring duplicate packet: {:?}", header.id());
            self.stats.packets_duplicate += 1;
            return Err(PicklebackError::DuplicatePacket);
        }
        let packet_len = header.size() + payload_reader.remaining() as usize;
        self.received_buffer.insert(
            header.id().0,
            ReceivedMeta::new(self.time, self.config.packet_header_size + packet_len),
        )?;
        self.process_packet_acks_and_rtt(header);
        // Only the messages packet type leaves stuff in the buffer for us to process
        if matches!(header.packet_type, PacketType::Messages) {
            // as long as there are bytes left to read, we should only find whole messages
            while payload_reader.remaining() > 0 {
                // this gets a pooled buffer, and must be returned once consumer has seen message TODO
                let msg = Message::parse(&mut self.pool, payload_reader)?;
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
        }
        Ok(())
    }

    /// Parses ack bitfield and checks for unseen acks, reports them to the dispatcher so it can
    /// ack the message ids associated with the packet sequence nunmber.
    /// Updates rtt calculations.
    fn process_packet_acks_and_rtt(&mut self, header: &ProtocolPacketHeader) {
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
                    if !matches!(header.packet_type, PacketType::KeepAlive) {
                        if let Some(lowest_ack) = self.ack_low_watermark_non_empty {
                            if SequenceBuffer::<u16>::sequence_greater_than(
                                sent_data.acked_up_to.0,
                                lowest_ack.0,
                            ) {
                                self.ack_low_watermark_non_empty = Some(sent_data.acked_up_to);
                            }
                        } else {
                            self.ack_low_watermark_non_empty = Some(sent_data.acked_up_to);
                        }
                    }
                    //
                    self.stats.packets_acked += 1;
                    sent_data.acked = true;
                    // this allows the dispatcher to ack the messages that were sent in this packet
                    let returned_buffers = self
                        .dispatcher
                        .acked_packet(PacketId(ack_sequence), &mut self.channels);
                    for returned_buf in returned_buffers {
                        self.pool.return_buffer(returned_buf);
                    }
                    // update rtt calculations
                    let rtt: f32 = (self.time - sent_data.time) as f32 * 1000.0;
                    log::info!(
                        "rtt = {rtt} header: {header:?} sent_data: {sent_data:?} self.time: {}",
                        self.time
                    );
                    if (self.rtt == 0.0 && rtt > 0.0) || (self.rtt - rtt).abs() < 0.00001 {
                        self.rtt = rtt;
                    } else {
                        self.rtt = self.rtt + ((rtt - self.rtt) * self.config.rtt_smoothing_factor);
                    }
                }
            }
        }
        if self.sequence_id % PACKET_LOSS_RECALCULATE_INTERVAL == 0 {
            self.update_packet_loss_calculation();
        }
    }

    /// Calculates packet loss by checking to see which packets sent longer ago than RTT*1.1 haven't
    /// been acked.
    fn update_packet_loss_calculation(&mut self) {
        // give a 10% buffer to rtt, only check for acks on packets sent longer ago than this.
        let sent_time_cutoff = self.time - self.rtt as f64 * 1.1;
        let mut num_acked = 0;
        let mut num_sampled = 0;
        let mut seq = self.sent_buffer.sequence();
        while let Some(meta) = self.sent_buffer.get(seq) {
            seq = seq.wrapping_sub(1);
            if meta.time > sent_time_cutoff {
                continue;
            }
            num_sampled += 1;
            if meta.acked() {
                num_acked += 1;
            }
            if num_sampled == PACKET_LOSS_RECALCULATE_INTERVAL {
                break;
            }
        }
        if num_sampled == 0 {
            warn!("Couldn't calculate packet loss, no samples");
            return;
        }
        let loss = 1.0 - num_acked as f32 / num_sampled as f32;
        self.packet_loss = self.packet_loss
            + ((loss - self.packet_loss) * self.config.packet_loss_smoothing_factor);
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
    pub fn config(&self) -> &PicklebackConfig {
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

        let mut server = Pickleback::default();
        let mut client = Pickleback::default();
        client.set_xor_salt(Some(0));
        server.set_xor_salt(Some(0));
        let payload = b"hello";
        let _msg_id = server.send_message(0, payload);

        let to_send = server.collect_packets_to_send_inefficiently();
        assert_eq!(to_send.len(), 1);

        info!("Sending first copy of packet");

        let mut cur = Cursor::new(to_send[0].as_slice());
        let ProtocolPacket::Messages(MessagesPacket { header, .. }) =
            read_packet(&mut cur).unwrap()
        else {
            panic!("err");
        };
        assert!(client.process_incoming_packet(&header, &mut cur).is_ok());

        {
            let mut received_messages_container = client.drain_received_messages(0);
            let rec = received_messages_container.next().unwrap();
            assert_eq!(rec.channel(), 0);
            assert_eq!(rec.payload_to_owned().as_slice(), payload.as_ref());
        }
        // send dupe
        info!("Sending second (dupe) copy of packet");
        let mut cur = Cursor::new(to_send[0].as_slice());
        let ProtocolPacket::Messages(MessagesPacket { header, .. }) =
            read_packet(&mut cur).unwrap()
        else {
            panic!("err");
        };
        match client.process_incoming_packet(&header, &mut cur) {
            Err(PicklebackError::DuplicatePacket) => {}
            e => {
                panic!("Should be dupe packet error, got: {:?}", e);
            }
        }
    }

    #[test]
    fn many_packets_in_flight() {
        crate::test_utils::init_logger();
        let mut server = Pickleback::default();
        let mut client = Pickleback::default();
        client.set_xor_salt(Some(0));
        server.set_xor_salt(Some(0));
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
        server.visit_packets_to_send(|packet| {
            let mut cur = Cursor::new(packet);
            let ProtocolPacket::Messages(MessagesPacket { header, .. }) =
                read_packet(&mut cur).unwrap()
            else {
                panic!("err");
            };
            client.process_incoming_packet(&header, &mut cur).unwrap();
        });
        info!("sent_unacked is now: {}", server.sent_unacked_packets());
        info!("Send attempt for MAX_UNACKED_PACKETS+1");
        match server.send_message(0, msg) {
            Err(PicklebackError::Backpressure(Backpressure::TooManyPending)) => {}
            other => panic!("Expecting backpressure, got {other:?}"),
        }

        client.update(1.0);
        // exchange packets
        info!(
            "Client sending to server.. sent_unacked is {}",
            client.sent_unacked_packets()
        );
        client.visit_packets_to_send(|packet| {
            let mut cur = Cursor::new(packet);
            let ProtocolPacket::Messages(MessagesPacket { header, .. }) =
                read_packet(&mut cur).unwrap()
            else {
                panic!("err");
            };
            server.process_incoming_packet(&header, &mut cur).unwrap();
        });

        assert_eq!(sent_ids.len(), client.drain_received_messages(0).len());
        assert_eq!(sent_ids.len(), server.drain_message_acks(0).len());
    }

    #[test]
    fn acks() {
        crate::test_utils::init_logger();

        let test_data = &[0x41; 24]; // "AAA..."

        let mut server = Pickleback::default();
        let mut client = Pickleback::default();
        client.set_xor_salt(Some(0));
        server.set_xor_salt(Some(0));

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
            server.visit_packets_to_send(|packet| {
                let mut cur = Cursor::new(packet);
                let ProtocolPacket::Messages(MessagesPacket { header, .. }) =
                    read_packet(&mut cur).unwrap()
                else {
                    panic!("err");
                };

                client.process_incoming_packet(&header, &mut cur).unwrap();
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
            client.visit_packets_to_send(|packet| {
                let mut cur = Cursor::new(packet);
                let ProtocolPacket::Messages(MessagesPacket { header, .. }) =
                    read_packet(&mut cur).unwrap()
                else {
                    panic!("err");
                };
                server.process_incoming_packet(&header, &mut cur).unwrap();
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
                .insert(i as u16, TestData { sequence: i as u16 })
                .unwrap();
            assert_eq!(buffer.sequence(), i as u16);

            let r = buffer.get(i as u16);
            assert_eq!(r.unwrap().sequence, i as u16);
        }

        for i in 0..TEST_BUFFER_SIZE - 1 {
            let r = buffer.insert(i as u16, TestData { sequence: i as u16 });
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
        let write_packet =
            ProtocolPacketHeader::new(write_id, ack_iter, 1, PacketType::Messages).unwrap();
        write_packet.write(&mut cur).unwrap();

        let mut reader = Cursor::new(buffer.as_ref());
        let read_packet = ProtocolPacketHeader::parse(&mut reader).unwrap();

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
        let mut harness = MessageTestHarness::new(JitterPipeConfig::disabled());

        let msg1 = b"Hello";
        let msg2 = b"world";
        let msg3 = b"!";
        let msg4 = b"";
        let id1 = harness.server.send_message(channel, msg1).unwrap();
        let id2 = harness.server.send_message(channel, msg2).unwrap();
        let id3 = harness.server.send_message(channel, msg3).unwrap();
        let id4 = harness.server.send_message(channel, msg4).unwrap();

        harness.advance(0.1);

        let received_messages_container = harness.client.drain_received_messages(channel);
        let received_messages = received_messages_container.messages();
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
        let mut harness = MessageTestHarness::new(JitterPipeConfig::disabled());

        let mut msg = Vec::new();
        msg.extend_from_slice(&[65; 1024]);
        msg.extend_from_slice(&[66; 1024]);
        msg.extend_from_slice(&[67; 100]);

        let msg_id = harness.server.send_message(channel, msg.as_ref()).unwrap();

        harness.advance(0.1);

        let received_messages_container = harness.client.drain_received_messages(channel);
        let received_messages = received_messages_container.messages();
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
        let mut pool = BufPool::empty();
        let channel = 0;
        let mut harness = MessageTestHarness::new(JitterPipeConfig::disabled());
        let payload = b"hello";
        harness
            .server
            .channels_mut()
            .get_mut(channel)
            .unwrap()
            .enqueue_message(&mut pool, MessageId(123), payload, Fragmented::No);
        harness
            .server
            .channels_mut()
            .get_mut(channel)
            .unwrap()
            .enqueue_message(&mut pool, MessageId(123), payload, Fragmented::No);
        harness.advance(1.);
        assert_eq!(harness.client.drain_received_messages(channel).len(), 1);
    }

    // test what happens in a reliable channel when one of the fragments isn't delivered.
    // should resend after a suitable amount of time.
    #[test]
    fn retransmission() {
        let channel = 1;
        let mut harness = MessageTestHarness::new(JitterPipeConfig::disabled());
        // big enough to require 2 packets
        let payload = random_payload(1800);
        let id = harness
            .server
            .send_message(channel, payload.as_ref())
            .unwrap();
        // drop second packet (index 1), which will be the second of the two fragments.
        harness.advance_with_server_outbound_drops(0.05, vec![1]);
        assert!(harness.drain_client_messages(channel).is_empty());
        assert!(harness.collect_server_acks(channel).is_empty());
        // retransmit not ready yet
        harness.advance(0.01);
        assert!(harness.drain_client_messages(channel).is_empty());
        assert!(harness.collect_server_acks(channel).is_empty());
        // should retransmit
        harness.advance(0.09001); // retransmit time of 0.1 reached
        assert_eq!(harness.drain_client_messages(channel).len(), 1);
        assert_eq!(vec![id], harness.collect_server_acks(channel));
        // ensure server finished sending:
        harness.advance(1.0);
        // this is testing that the server is only transmitting one small "empty" packet (just headers)
        let to_send = harness.server.collect_packets_to_send_inefficiently();
        assert_eq!(1, to_send.len());
        // both our fragments are def bigger than 50:
        assert!(to_send[0].len() < 50);
    }
}
