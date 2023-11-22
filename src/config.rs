/// packet_header_size is the bytes overhead when sending a UDP packet, used to calcualte
/// actual bandwidth used.
///
/// UDP over IPv4 = 20 + 8 bytes, UDP over IPv6 = 40 + 8 bytes.
///
///
#[derive(Clone)]
pub struct PacketeerConfig {
    /// Maximum size of a message payload.
    /// Messages larger than `max_packet_size` are automatically fragmented
    pub max_message_size: usize,
    /// Max size of a packet you can send without fragmenting. ~1200 bytes or so.
    pub max_packet_size: usize,
    /// How many sent packets do we track state for?
    /// Only sent-packets still in this buffer can be successfully acked.
    /// So consider how many packets will be in-flight/unacked between two endpoints.
    pub sent_packets_buffer_size: usize,
    /// This buffer prevents receiving duplicate packets, and is used for assembling the
    /// ack field sent to the remote endpoint.
    pub received_packets_buffer_size: usize,
    /// A newly calculated rtt is blended with the existing rtt using this smoothing factor between 0-1
    /// to avoid large jumps.
    pub rtt_smoothing_factor: f32,
    // pub packet_loss_smoothing_factor: f32,
    // pub bandwidth_smoothing_factor: f32,
    /// An estimate of the header size used to transmit packets, used to calculate true bandwidth usage.
    /// UDP over IPv4 is ~28, and UDP over IPv6 is ~48.
    pub packet_header_size: usize,
}

impl Default for PacketeerConfig {
    fn default() -> Self {
        Self {
            max_message_size: 1024 * 1024,
            max_packet_size: 1150,
            sent_packets_buffer_size: 256,
            received_packets_buffer_size: 256,
            rtt_smoothing_factor: 0.0025,
            // packet_loss_smoothing_factor: 0.1,
            // bandwidth_smoothing_factor: 0.1,
            packet_header_size: 28,
        }
    }
}
