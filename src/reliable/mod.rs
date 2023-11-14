use log::*;

use std::collections::VecDeque;
// use std::num::Wrapping;
mod sequence_buffer;

pub use sequence_buffer::SequenceBuffer;

mod error;
pub use error::ReliableError;

mod headers;
use bytes::{Bytes, BytesMut};
pub use headers::HeaderParser as Header;
pub use headers::PacketHeader;

// pub const RELIABLE_MAX_PACKET_HEADER_BYTES: usize = 9;
// pub const RELIABLE_FRAGMENT_HEADER_BYTES: usize = 5;

#[derive(Clone)]
pub struct EndpointConfig {
    pub max_payload_size: usize,
    pub max_packet_size: usize,
    pub sent_packets_buffer_size: usize,
    pub received_packets_buffer_size: usize,
    pub rtt_smoothing_factor: f32,
    pub packet_loss_smoothing_factor: f32,
    pub bandwidth_smoothing_factor: f32,
    pub packet_header_size: usize,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            max_payload_size: 1014,
            max_packet_size: 1150,
            sent_packets_buffer_size: 256,
            received_packets_buffer_size: 256,
            rtt_smoothing_factor: 0.0025,
            packet_loss_smoothing_factor: 0.1,
            bandwidth_smoothing_factor: 0.1,
            packet_header_size: 28, // note: UDP over IPv4 = 20 + 8 bytes, UDP over IPv6 = 40 + 8 bytes
        }
    }
}

#[derive(Debug, Clone)]
pub struct SentData {
    time: f64,
    acked: bool,
    size: usize,
}

impl SentData {
    pub fn new(time: f64, size: usize) -> Self {
        Self {
            time,
            size,
            acked: false,
        }
    }
    #[allow(unused)]
    pub fn acked(&self) -> bool {
        self.acked
    }
    #[allow(unused)]
    pub fn size(&self) -> usize {
        self.size
    }
}

impl Default for SentData {
    fn default() -> Self {
        Self {
            time: 0.0,
            size: 0,
            acked: false,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct RecvData {
    time: f64,
    size: usize,
}
impl RecvData {
    pub fn new(time: f64, size: usize) -> Self {
        Self { time, size }
    }
}

impl Default for RecvData {
    fn default() -> Self {
        Self { time: 0.0, size: 0 }
    }
}

#[derive(Default, Debug, Clone)]
pub struct EndpointCounters {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_acked: u64,
    pub packets_stale: u64,
    pub packets_duplicate: u64,
    pub packets_invalid: u64,
    pub packets_too_large_to_send: u64,
    pub packets_too_large_to_receive: u64,
}

/// returned from send - contains packet seqno
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct SentHandle(u16);

#[derive(Debug)]
pub struct ReceivedPacket {
    pub handle: SentHandle,
    pub payload: Bytes,
    pub acks: Vec<SentHandle>,
}

pub struct Endpoint {
    time: f64,
    rtt: f32,
    config: EndpointConfig,
    sequence: u16,
    sent_buffer: SequenceBuffer<SentData>,
    recv_buffer: SequenceBuffer<RecvData>,
    counters: EndpointCounters,
    outbox: VecDeque<Bytes>,
}

impl Endpoint {
    pub fn new(config: EndpointConfig, time: f64) -> Self {
        Self {
            time,
            rtt: 0.0,
            config: config.clone(),
            sequence: 0,
            sent_buffer: SequenceBuffer::with_capacity(config.sent_packets_buffer_size),
            recv_buffer: SequenceBuffer::with_capacity(config.received_packets_buffer_size),
            counters: EndpointCounters::default(),
            outbox: VecDeque::new(),
        }
    }

    // pub fn max_payload_size(&self) -> usize {
    //     self.config.max_payload_size
    // }

    /// draining iterator over packets in the outbox that we need to send over the network
    pub fn drain_packets_to_send(
        &mut self,
    ) -> std::collections::vec_deque::Drain<'_, bytes::Bytes> {
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
    pub fn sent_info(&self, sent_handle: SentHandle) -> Option<&SentData> {
        self.sent_buffer.get(sent_handle.0)
    }

    /// Packetizes payload by adding approriate headers, writes resulting packet to outbox.
    /// Payload must be within the max_payload_size - ie, under the MTU. No fragmenting here.
    /// Returns SendHandle, a wrapper over the packet sequence number
    pub fn send(&mut self, payload: Bytes) -> Result<SentHandle, ReliableError> {
        if payload.len() > self.config.max_packet_size {
            panic!(
                "Packet too large: Attempting to send {}, {:?} max={}\n{payload:?}",
                payload.len(),
                ReliableError::ExceededMaxPacketSize,
                self.config.max_packet_size
            );
            // self.counters.packets_too_large_to_send += 1;
            // return Err(ReliableError::ExceededMaxPacketSize);
        }

        self.sequence = self.sequence.wrapping_add(1);
        let sequence = self.sequence;

        let (ack, ack_bits) = self.recv_buffer.ack_bits();

        let send_size = payload.len() + self.config.packet_header_size;
        let sent = SentData::new(self.time, send_size);
        self.sent_buffer.insert(sent, sequence)?;

        let header = PacketHeader::new(sequence, ack, ack_bits);

        debug!(
            ">>> Sending packet seq:{} ack:{} ack_bits:{:#0b}",
            header.sequence(),
            header.ack(),
            header.ack_bits()
        );
        let mut new_packet = BytesMut::with_capacity(header.size() + payload.len());
        header.write(&mut new_packet)?;
        new_packet.extend_from_slice(payload.as_ref());
        self.outbox.push_back(new_packet.freeze());

        self.counters.packets_sent += 1;
        Ok(SentHandle(sequence))
    }

    /// Give me all the udp packets you read from the network.
    /// You get back acks and the received packet stripped of our headers.
    #[allow(clippy::cast_possible_truncation)]
    pub fn receive(&mut self, mut packet: Bytes) -> Result<ReceivedPacket, ReliableError> {
        self.counters.packets_received += 1;

        let header: PacketHeader = PacketHeader::parse(&mut packet)?;
        // trace!(
        //     "Receiving packet, t:{} seq:{} recv_buf.seq:{}",
        //     self.time,
        //     header.sequence(),
        //     self.recv_buffer.sequence()
        // );
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
        // could maintain a drainable member vec of acked handles, and drain it
        // and convert on the fly when draining at the packeteer level?
        // would remove the vec allocation here each time.

        // walk the ack bits and ack anything we haven't already acked, and collect the handles
        let mut ack_bits = header.ack_bits();
        let mut new_acks = Vec::new();
        // create ack field for last 32 msgs
        for i in 0..32 {
            if ack_bits & 1 != 0 {
                let ack_sequence = header.ack().wrapping_sub(i);

                if let Some(sent_data) = self.sent_buffer.get_mut(ack_sequence) {
                    if !sent_data.acked {
                        self.counters.packets_acked += 1;
                        sent_data.acked = true;
                        new_acks.push(SentHandle(ack_sequence));

                        let rtt: f32 = (self.time - sent_data.time) as f32 * 1000.0;
                        if (self.rtt == 0.0 && rtt > 0.0) || (self.rtt - rtt).abs() < 0.00001 {
                            self.rtt = rtt;
                        } else {
                            self.rtt =
                                self.rtt + ((rtt - self.rtt) * self.config.rtt_smoothing_factor);
                        }
                    }
                    // } else {
                    // warn!("Why are we getting an ack for a seq we don't have a send_data for?");
                }
            }
            ack_bits >>= 1;
        }
        let received_packet = ReceivedPacket {
            handle: SentHandle(header.sequence()),
            payload: packet,
            acks: new_acks,
        };
        Ok(received_packet)
    }

    pub fn update(&mut self, dt: f64) {
        self.time += dt;
    }

    #[allow(unused)]
    pub fn counters(&self) -> &EndpointCounters {
        &self.counters
    }
}

#[cfg(test)]
mod tests {
    const TEST_BUFFER_SIZE: usize = 256;

    use super::*;
    // explicit import to override bevy

    use crate::test_utils::*;
    use log::info;

    #[test]
    fn duplicates_dropped() {
        init_logger();

        let mut server = Endpoint::new(EndpointConfig::default(), 1.);
        let mut client = Endpoint::new(EndpointConfig::default(), 1.);
        let payload = Bytes::from_static(b"hello");
        let SentHandle(seq) = server.send(payload.clone()).unwrap();

        let to_send = server.drain_packets_to_send().collect::<Vec<_>>();
        assert_eq!(to_send.len(), 1);

        info!("Sending first copy of packet");
        let rp = client.receive(to_send[0].clone()).unwrap();
        assert_eq!(rp.handle.0, seq);
        assert_eq!(rp.payload, payload);

        // send dupe
        info!("Sending second (dupe) copy of packet");
        match client.receive(to_send[0].clone()) {
            Err(ReliableError::DuplicatePacket) => {}
            e => {
                panic!("Should be dupe packet error, got: {:?}", e);
            }
        }
    }

    const TEST_ACKS_NUM_ITERATIONS: usize = 200;
    #[test]
    fn acks() {
        crate::test_utils::init_logger();

        let time = 100.0;
        let test_data = Bytes::copy_from_slice(&[0x41; 24]); // "AAA..."

        let mut one = Endpoint::new(EndpointConfig::default(), time);
        let mut two = Endpoint::new(EndpointConfig::default(), time);

        let delta_time = 0.01;
        let mut one_sent = Vec::new();
        let mut two_sent = Vec::new();

        for _ in 0..TEST_ACKS_NUM_ITERATIONS {
            log::info!("tick..");
            // Send test packets
            let handle1 = one.send(test_data.clone()).unwrap();
            let handle2 = two.send(test_data.clone()).unwrap();

            one_sent.push(handle1);
            two_sent.push(handle2);

            // forward packets to their endpoints
            one.drain_packets_to_send().for_each(|packet| {
                info!("Sending one -> two: {packet:?}");
                let rp = two.receive(packet).unwrap();
                info!("two received: {rp:?}");
                assert_eq!(test_data, rp.payload);
            });
            assert!(!one.has_packets_to_send());
            two.drain_packets_to_send().for_each(|packet| {
                info!("Sending two -> one: {packet:?}");
                let rp = one.receive(packet).unwrap();
                info!("one received: {rp:?}");
                assert_eq!(test_data, rp.payload);
            });
            assert!(!two.has_packets_to_send());

            one.update(delta_time);
            two.update(delta_time);
        }
    }

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
}
