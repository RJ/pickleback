use crate::MAX_FRAGMENTS;

/// Various tunables. Mostly buffer sizes.
#[derive(Clone)]
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Resource))]
pub struct PicklebackConfig {
    /// Optionally specify buffer pool sizes and capacities.
    /// Default value of None will give sane defaults.
    pub buffer_pools_config: Option<Vec<crate::buffer_pool::PoolConfig>>,
    /// Maximum size of a message payload.
    /// Messages larger than `max_packet_size` are automatically fragmented
    /// NB: at the mo, MAX_FRAGMENTS is hardcoded, so this must not exceed
    ///     MAX_FRAGMENTS * 1024 !
    pub max_message_size: usize,
    /// Max size of a packet you can send without fragmenting. ~1200 bytes or so.
    pub max_packet_size: usize,
    /// How many sent packets do we track state for?
    /// Only sent-packets still in this buffer can be successfully acked.
    /// So consider how many packets will be in-flight/unacked between two endpoints.
    pub sent_packets_buffer_size: usize,
    /// This buffer prevents receiving duplicate packets, and is used for assembling the
    /// ack field sent to the remote endpoint.
    /// Also the number of previous packets we retain message-id mappings for, for acks.
    pub received_packets_buffer_size: usize,
    /// A newly calculated rtt is blended with the existing value using this smoothing factor between 0-1
    /// to avoid large jumps.
    pub rtt_smoothing_factor: f32,
    /// A newly calculated packet loss is blended with the existing value using this smoothing factor between 0-1
    /// to avoid large jumps.
    pub packet_loss_smoothing_factor: f32,
    // pub bandwidth_smoothing_factor: f32,
    /// An estimate of the header size used to transmit packets, used to calculate true bandwidth usage.
    /// UDP over IPv4 is ~28, and UDP over IPv6 is ~48.
    pub packet_header_size: usize,
    /// For senders to determine if a sent fragmented message is acked, they need to track the
    /// ack status of each fragment message. Once all fragments acked, ack the parent message id.
    /// This is the size of the fragment message id sequence buffer for tracking fragment acks.
    /// This is a sparsely filled buffer, because often non-fragmented messages make up the bulk of
    /// transmission. So it needs to be fairly large to support multiple in-flight/unacked fragmented messages.
    ///
    /// A fairly extreme scenario:
    /// Very small unfragmented messages might fit 100 to a packet, and perhaps you're sending
    /// 100 packets per second. That's 10,000 msg ids per second.
    /// So you need a buffer size of 10,000 to support acks of sent fragmented messages up to
    /// a second after transmission, because even unfragmented message ids consume a slot in the map
    /// due to how sequence buffers work.
    ///
    /// Setting a high default. Tune it down if for lower msg/sec send rates accordingly.
    pub sent_frag_map_size: usize,
}

impl Default for PicklebackConfig {
    fn default() -> Self {
        Self {
            buffer_pools_config: None,
            max_message_size: 1024 * MAX_FRAGMENTS,
            max_packet_size: 1150,
            sent_packets_buffer_size: 512,
            received_packets_buffer_size: 512,
            rtt_smoothing_factor: 0.0025,
            packet_header_size: 28,
            packet_loss_smoothing_factor: 0.5,
            sent_frag_map_size: 25000,
        }
    }
}
