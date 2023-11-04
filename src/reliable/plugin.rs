use crate::*;
use bevy::prelude::*;

pub struct ReliablePlugin(pub EndpointConfig);

#[derive(Debug, Default, Resource)]
pub struct EndpointCounters {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_acked: u64,
    pub packets_stale: u64,
    pub packets_invalid: u64,
    pub packets_too_large_to_send: u64,
    pub packets_too_large_to_receive: u64,
    pub fragments_sent: u64,
    pub fragments_received: u64,
    pub fragments_invalid: u64,
}

#[derive(Debug, Clone, Resource)]
struct EndpointConfig {
    pub max_packet_size: usize,
    pub fragment_above: usize,
    pub max_fragments: u32,
    pub fragment_size: usize,
    pub ack_buffer_size: usize,
    pub sent_packets_buffer_size: usize,
    pub received_packets_buffer_size: usize,
    pub fragment_reassembly_buffer_size: usize,
    pub rtt_smoothing_factor: f32,
    pub packet_loss_smoothing_factor: f32,
    pub bandwidth_smoothing_factor: f32,
    pub packet_header_size: usize,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            max_packet_size: 16 * 1024,
            fragment_above: 1024,
            max_fragments: 16,
            fragment_size: 1024,
            ack_buffer_size: 256,
            sent_packets_buffer_size: 256,
            received_packets_buffer_size: 256,
            fragment_reassembly_buffer_size: 64,
            rtt_smoothing_factor: 0.0025,
            packet_loss_smoothing_factor: 0.1,
            bandwidth_smoothing_factor: 0.1,
            packet_header_size: 28,
        }
    }
}

#[derive(Event)]
pub struct Packetish {
    sequence: u16,
}

#[derive(Resource)]
pub struct Endpoint {
    // time: f64,
    rtt: f32,
    config: EndpointConfig,
    acks: Vec<u16>,
    sequence: i32,
    sent_buffer: SequenceBuffer<SentData>,
    recv_buffer: SequenceBuffer<RecvData>,
    reassembly_buffer: SequenceBuffer<ReassemblyData>,
    temp_packet_buffer: Vec<u8>,
    // counters: EndpointCounters,
}

impl Endpoint {}

impl Plugin for ReliablePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EndpointConfig>();
        let config = app.world.get_resource::<EndpointConfig>().unwrap().clone();
        let endpoint = Endpoint {
            rtt: 0.0,
            config: config.clone(),
            acks: Vec::with_capacity(config.ack_buffer_size),
            sequence: 0,
            sent_buffer: SequenceBuffer::with_capacity(config.sent_packets_buffer_size),
            recv_buffer: SequenceBuffer::with_capacity(config.received_packets_buffer_size),
            reassembly_buffer: SequenceBuffer::with_capacity(
                config.fragment_reassembly_buffer_size,
            ),
            temp_packet_buffer: Vec::with_capacity(config.max_packet_size),
        };
        app.insert_resource(endpoint);
        app.insert_resource(EndpointCounters::default());
    }
}

impl ReliablePlugin {}
