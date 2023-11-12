use log::*;

use std::collections::VecDeque;
use std::num::Wrapping;
use std::time::Instant;

mod sequence_buffer;
pub use sequence_buffer::SequenceBuffer;

mod error;
pub use error::ReliableError;

mod headers;
pub use headers::HeaderParser as Header;
pub use headers::PacketHeader;

use bytes::{Bytes, BytesMut};

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
    sequence: i32,
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

        // Increment sequence
        let sequence = self.sequence;
        self.sequence += 1;

        let (ack, ack_bits) = self.recv_buffer.ack_bits();

        let send_size = payload.len() + self.config.packet_header_size;
        let sent = SentData::new(self.time, send_size); // time, acked, size.
        self.sent_buffer.insert(sent, sequence as u16)?;

        let header = PacketHeader::new(sequence as u16, ack, ack_bits);

        trace!("Sending packet {}", sequence);
        let mut new_packet = BytesMut::with_capacity(header.size() + payload.len());
        header.write(&mut new_packet)?;
        new_packet.extend_from_slice(payload.as_ref());
        self.outbox.push_back(new_packet.freeze());

        self.counters.packets_sent += 1;
        Ok(SentHandle(sequence as u16))
    }

    /// Give me all the udp packets you read from the network.
    /// You get back acks and the received packet stripped of our headers.
    #[allow(clippy::cast_possible_truncation)]
    pub fn receive(&mut self, mut packet: Bytes) -> Result<ReceivedPacket, ReliableError> {
        self.counters.packets_received += 1;

        let header: PacketHeader = PacketHeader::parse(&mut packet)?;

        if !self.recv_buffer.check_sequence(header.sequence()) {
            error!("Ignoring stale packet: {}", header.sequence());
            self.counters.packets_stale += 1;
            return Err(ReliableError::StalePacket);
        }
        trace!("Processing packet...");

        self.recv_buffer.insert(
            RecvData::new(self.time, self.config.packet_header_size + packet.len()),
            header.sequence(),
        )?;

        let mut ack_bits = header.ack_bits();
        let mut new_acks = Vec::new();
        for i in 0..32 {
            if ack_bits & 1 != 0 {
                let ack_sequence: u16 = (Wrapping(header.ack()) - Wrapping(i)).0;

                if let Some(sent_data) = self.sent_buffer.get_mut(ack_sequence) {
                    if !sent_data.acked {
                        trace!("mark acked packet: {}", ack_sequence);
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
    pub fn reset(&mut self) {
        self.sequence = 0;
        self.sent_buffer.reset();
        self.recv_buffer.reset();
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
    use log::info;

    const TEST_ACKS_NUM_ITERATIONS: usize = 200;
    #[test]
    fn acks() {
        crate::test_utils::init_logger();

        let mut time = 100.0;
        let test_data = Bytes::copy_from_slice(&[0x41; 24]); // "AAA..."

        let mut one = Endpoint::new(EndpointConfig::default(), time);
        let mut two = Endpoint::new(EndpointConfig::default(), time);

        let delta_time = 0.01;
        let mut one_sent = Vec::new();
        let mut two_sent = Vec::new();

        for _ in 0..TEST_ACKS_NUM_ITERATIONS {
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
            two.drain_packets_to_send().for_each(|packet| {
                info!("Sending two -> one: {packet:?}");
                let rp = one.receive(packet).unwrap();
                info!("one received: {rp:?}");
                assert_eq!(test_data, rp.payload);
            });

            time += delta_time;
            one.update(time);
            two.update(time);
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

        assert_eq!(ack, TEST_BUFFER_SIZE as u16);
        assert_eq!(ack_bits, 0xFFFFFFFF);

        ////

        buffer.reset();

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

        assert_eq!(ack, 11);
        assert_eq!(
            ack_bits,
            (1 | (1 << (11 - 9)) | (1 << (11 - 5)) | (1 << (11 - 1)))
        );
    }

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
            assert_eq!(buffer.sequence(), i as u16 + 1);

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
