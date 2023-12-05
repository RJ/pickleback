use crate::*;
use enum_dispatch::*;

#[enum_dispatch]
pub(crate) trait ChannelT {
    fn id(&self) -> u8;
    /// Update the channel clock. Necessary so it knows when to retransmit.
    fn update(&mut self, time: f64);
    /// Any messages ready to send in the next packet?
    fn any_ready_to_send(&self) -> bool;
    /// enqueue a message to be sent in an outbound packet
    fn enqueue_message(
        &mut self,
        pool: &BufPool,
        id: MessageId,
        payload: &[u8],
        fragmented: Fragmented,
    );
    /// called when a message has been acked by the packet layer
    fn message_ack_received(&mut self, msg_handle: &MessageHandle);
    /// get message ready to be coalesced into an outbound packet
    fn get_message_to_write_to_a_packet(&mut self, max_size: usize) -> Option<Message>;
    // false if we've already fully received this message id, or it's known to be stale/useless
    fn accepts_message(&mut self, msg: &Message) -> bool;
}

#[enum_dispatch(ChannelT)]
pub(crate) enum Channel {
    UnreliableChannel,
    ReliableChannel,
}

pub(crate) struct UnreliableChannel {
    time: f64,
    pub(crate) id: u8,
    q: VecDeque<Message>,
    seen_buf: SequenceBuffer<bool>,
}

impl UnreliableChannel {
    pub(crate) fn new(id: u8, time: f64) -> Self {
        Self {
            id,
            time,
            q: VecDeque::default(),
            seen_buf: SequenceBuffer::with_capacity(10000),
        }
    }
}

impl ChannelT for UnreliableChannel {
    fn update(&mut self, dt: f64) {
        self.time += dt;
    }
    // need to not recv duplicate msgs or fragments somehow. fragmap knows..
    fn accepts_message(&mut self, msg: &Message) -> bool {
        if !self.seen_buf.check_sequence(msg.id().0) {
            warn!("Rejecting too-old message on chanel {msg:?}");
            return false;
        }

        if self.seen_buf.exists(msg.id().0) {
            // warn!("Rejecting already seen message on channel {msg:?}");
            return false;
        }
        match self.seen_buf.insert(msg.id().0, true) {
            Ok(_) => true,
            Err(e) => {
                warn!("not accepting message {msg:?} = {e:?}");
                false
            }
        }
    }
    fn enqueue_message(
        &mut self,
        pool: &BufPool,
        id: MessageId,
        payload: &[u8],
        fragmented: Fragmented,
    ) {
        let msg = Message::new_outbound(pool, id, self.id(), payload, fragmented);
        info!(">>>>> unreliable chan enq msg: {msg:?}");
        self.q.push_back(msg);
    }
    fn any_ready_to_send(&self) -> bool {
        !self.q.is_empty()
    }
    fn id(&self) -> u8 {
        self.id
    }
    fn message_ack_received(&mut self, _handle: &MessageHandle) {}
    // this removes after returning, but a reliable queue shouldn't until acked.
    fn get_message_to_write_to_a_packet(&mut self, max_size: usize) -> Option<Message> {
        for index in 0..self.q.len() {
            if self.q[index].size() > max_size {
                continue;
            }
            return self.q.remove(index);
        }
        None
    }
}

struct ResendableMessage {
    message: Message,
    last_sent: Option<f64>,
}
impl ResendableMessage {
    fn new(message: Message) -> Self {
        Self {
            message,
            last_sent: None,
        }
    }
    fn is_ready(&self, now: f64, cutoff: f64) -> bool {
        if self.last_sent.is_none() || (now - self.last_sent.unwrap()) >= cutoff {
            return true;
        }
        false
    }
    /// true if message id matches, or this message is a fragment of the acked id
    fn dismissed_by_ack(&self, acked_handle: &MessageHandle) -> bool {
        if self.message.id() != acked_handle.id() {
            return false;
        }
        if let Some(fragment) = self.message.fragment() {
            Some(fragment.parent_id) == acked_handle.parent_id()
        } else {
            // non-fragmented messages are sole users of their message id.
            assert!(acked_handle.frag_index.is_none());
            true
        }
    }
}

pub(crate) struct ReliableChannel {
    time: f64,
    pub(crate) id: u8,
    q: VecDeque<ResendableMessage>,
    resend_time: f64,
    seen_buf: SequenceBuffer<bool>,
}

impl ReliableChannel {
    pub(crate) fn new(id: u8, time: f64) -> Self {
        Self {
            id,
            time,
            q: VecDeque::default(),
            resend_time: 0.1,
            seen_buf: SequenceBuffer::with_capacity(10000),
        }
    }
}

impl ChannelT for ReliableChannel {
    fn update(&mut self, dt: f64) {
        self.time += dt;
    }
    fn accepts_message(&mut self, msg: &Message) -> bool {
        if !self.seen_buf.check_sequence(msg.id().0) {
            warn!("Rejecting too-old message on chanel {msg:?}");
            return false;
        }
        if self.seen_buf.exists(msg.id().0) {
            // warn!("Rejecting already seen message on channel {msg:?}");
            return false;
        }
        match self.seen_buf.insert(msg.id().0, true) {
            Ok(_) => true,
            Err(e) => {
                warn!("not accepting message {msg:?} = {e:?}");
                false
            }
        }
    }
    fn enqueue_message(
        &mut self,
        pool: &BufPool,
        id: MessageId,
        payload: &[u8],
        fragmented: Fragmented,
    ) {
        let msg = Message::new_outbound(pool, id, self.id(), payload, fragmented);
        self.q.push_back(ResendableMessage::new(msg));
    }
    fn any_ready_to_send(&self) -> bool {
        let now = self.time;
        let cutoff = self.resend_time;
        self.q.iter().any(|m| m.is_ready(now, cutoff))
    }
    fn id(&self) -> u8 {
        self.id
    }
    fn message_ack_received(&mut self, msg_handle: &MessageHandle) {
        self.q.retain(|m| !m.dismissed_by_ack(msg_handle));
    }
    // this leaves the messges in the queue until acked, but updates their last_sent time.
    fn get_message_to_write_to_a_packet(&mut self, max_size: usize) -> Option<Message> {
        for index in 0..self.q.len() {
            let re_msg = self.q.get_mut(index).unwrap();
            if re_msg.message.size() > max_size {
                continue;
            }
            if re_msg.is_ready(self.time, self.resend_time) {
                if re_msg.last_sent.is_some() {
                    info!("resending.. {:?}", re_msg.message.fragment());
                }
                re_msg.last_sent = Some(self.time);
                return Some(re_msg.message.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    // use crate::jitter_pipe::JitterPipeConfig;

    use super::*;
    // explicit import to override bevy
    // use log::{debug, error, info, trace, warn};

    #[test]
    fn unreliable_channel() {
        crate::test_utils::init_logger();
        let pool = BufPool::empty();
        let mut channel = UnreliableChannel::new(0, 1.0);
        let payload = b"hello";
        channel.enqueue_message(&pool, MessageId(0), payload, Fragmented::No);
        assert!(channel.any_ready_to_send());
        assert!(channel.get_message_to_write_to_a_packet(999999).is_some());
        channel.update(1.0);
        assert!(!channel.any_ready_to_send());
    }

    #[test]
    fn reliable_channel() {
        crate::test_utils::init_logger();
        let pool = BufPool::empty();
        let channel_id = 0;
        let mut channel = ReliableChannel::new(channel_id, 1.0);
        let payload = b"hello";
        let message_id = MessageId(123);
        channel.enqueue_message(&pool, message_id, payload, Fragmented::No);
        assert!(channel.any_ready_to_send());
        assert!(channel.get_message_to_write_to_a_packet(999999).is_some());
        // not empty, because non-acked
        assert!(!channel.any_ready_to_send());
        // advance enough time to trigger a resend
        channel.update(1.0);
        assert!(channel.any_ready_to_send());
        assert!(channel.get_message_to_write_to_a_packet(999999).is_some());
        let handle = MessageHandle {
            id: message_id,
            frag_index: None,
            channel: channel_id,
        };
        channel.message_ack_received(&handle);
        // advance enough time to trigger a resend
        channel.update(1.0);
        // none available, the ack removed it
        assert!(!channel.any_ready_to_send());
    }
}

#[derive(Default)]
pub(crate) struct ChannelList {
    channels: smallmap::Map<u8, Channel>,
}
impl ChannelList {
    pub(crate) fn get_mut(&mut self, id: u8) -> Option<&mut Channel> {
        self.channels.get_mut(&id)
    }
    pub(crate) fn insert(&mut self, channel: Channel) {
        assert!(
            (channel.id() as usize) < MAX_CHANNELS,
            "channel.id exceeds max"
        );
        self.channels.insert(channel.id(), channel);
    }
    pub(crate) fn all_mut(&mut self) -> impl Iterator<Item = &mut Channel> {
        self.channels.values_mut()
    }
    pub(crate) fn all_non_empty_mut(&mut self) -> impl Iterator<Item = &mut Channel> {
        self.channels.values_mut().filter(|c| c.any_ready_to_send())
    }
    pub(crate) fn any_with_messages_to_send(&self) -> bool {
        self.channels.values().any(|ch| ch.any_ready_to_send())
    }
}
