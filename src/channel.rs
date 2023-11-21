use crate::*;

// resend time could be dynamic based on recent rtt calculations per client?

// #[derive(Debug, Clone)]
// pub struct ChannelConfig {
//     id: u8,
//     reliable: bool,
//     ordered: bool,
//     resend_time: Option<Duration>,
// }

// pub enum DefaultChannels {
//     Unreliable,
//     Reliable,
// }

// impl From<DefaultChannels> for u8 {
//     fn from(value: DefaultChannels) -> Self {
//         match value {
//             DefaultChannels::Unreliable => 0,
//             DefaultChannels::Reliable => 1,
//         }
//     }
// }

// impl DefaultChannels {
//     pub fn channel_config() -> Vec<ChannelConfig> {
//         vec![
//             ChannelConfig {
//                 id: 0,
//                 reliable: false,
//                 ordered: false,
//                 resend_time: None,
//             },
//             ChannelConfig {
//                 id: 1,
//                 reliable: true,
//                 ordered: false,
//                 resend_time: Some(Duration::from_millis(100)),
//             },
//         ]
//     }
// }

// in lieu of a HashMap<u8,T>, since we have a fixed small number of channel ids
pub(crate) struct ChannelIdMap<T>([Option<T>; 32]);
impl<T> Default for ChannelIdMap<T> {
    fn default() -> Self {
        Self(std::array::from_fn(|_| None))
    }
}
impl<T> ChannelIdMap<T> {
    fn get_mut(&mut self, id: u8) -> Option<&mut T> {
        self.0.get_mut(id as usize).unwrap().as_mut()
    }
    fn insert(&mut self, id: u8, value: T) {
        assert!((id as usize) < self.0.len(), "channel id out of bounds");
        self.0[id as usize] = Some(value);
    }
    fn all(&self) -> impl Iterator<Item = &T> {
        self.0.iter().filter_map(|c| c.as_ref())
    }
    fn all_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.0.iter_mut().filter_map(|c| c.as_mut())
    }
}

type BoxedChannel = Box<dyn Channel>;

#[derive(Default)]
pub(crate) struct ChannelList {
    channels: ChannelIdMap<BoxedChannel>,
}
impl ChannelList {
    pub(crate) fn get_mut(&mut self, id: u8) -> Option<&mut Box<dyn Channel>> {
        self.channels.get_mut(id)
    }
    pub(crate) fn put(&mut self, channel: Box<dyn Channel>) {
        self.channels.insert(channel.id(), channel);
    }
    pub(crate) fn all_mut(&mut self) -> impl Iterator<Item = &mut BoxedChannel> {
        self.channels.all_mut()
    }
    pub(crate) fn all_non_empty_mut(&mut self) -> impl Iterator<Item = &mut BoxedChannel> {
        self.channels.all_mut().filter(|c| c.any_ready_to_send())
    }
    pub(crate) fn any_with_messages_to_send(&self) -> bool {
        for ch in self.channels.all() {
            if ch.any_ready_to_send() {
                return true;
            }
        }
        false
    }
}
pub(crate) trait Channel {
    fn id(&self) -> u8;
    fn update(&mut self, time: f64);
    fn any_ready_to_send(&self) -> bool;
    /// enqueue a message to be sent in an outbound packet
    fn enqueue_message(
        &mut self,
        pool: &BufPool,
        id: MessageId,
        payload: &[u8],
        fragmented: message::Fragmented,
    );
    /// called when a message has been acked by the packet layer
    fn message_ack_received(&mut self, msg_handle: &MessageHandle);
    /// get message ready to be coalesced into an outbound packet
    fn get_message_to_write_to_a_packet(&mut self, max_size: usize) -> Option<Message>;
    // false if we've already fully received this message id, or it's known to be stale/useless
    fn accepts_message(&mut self, msg: &Message) -> bool;
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

impl Channel for UnreliableChannel {
    fn update(&mut self, dt: f64) {
        self.time += dt;
    }
    // need to not recv duplicate msgs or fragments somehow. fragmap knows..
    fn accepts_message(&mut self, msg: &Message) -> bool {
        if !self.seen_buf.check_sequence(msg.id()) {
            warn!("Rejecting too-old message on chanel {msg:?}");
            return false;
        }
        if self.seen_buf.exists(msg.id()) {
            // warn!("Rejecting already seen message on channel {msg:?}");
            return false;
        }
        match self.seen_buf.insert(true, msg.id()) {
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
        fragmented: message::Fragmented,
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

impl Channel for ReliableChannel {
    fn update(&mut self, dt: f64) {
        self.time += dt;
    }
    fn accepts_message(&mut self, msg: &Message) -> bool {
        if !self.seen_buf.check_sequence(msg.id()) {
            warn!("Rejecting too-old message on chanel {msg:?}");
            return false;
        }
        if self.seen_buf.exists(msg.id()) {
            // warn!("Rejecting already seen message on channel {msg:?}");
            return false;
        }
        match self.seen_buf.insert(true, msg.id()) {
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
        fragmented: message::Fragmented,
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
        let pool = BufPool::new();
        let mut channel = UnreliableChannel::new(0, 1.0);
        let payload = b"hello";
        channel.enqueue_message(&pool, 0, payload, Fragmented::No);
        assert!(channel.any_ready_to_send());
        assert!(channel.get_message_to_write_to_a_packet(999999).is_some());
        channel.update(1.0);
        assert!(!channel.any_ready_to_send());
    }

    #[test]
    fn reliable_channel() {
        crate::test_utils::init_logger();
        let pool = BufPool::new();
        let channel_id = 0;
        let mut channel = ReliableChannel::new(channel_id, 1.0);
        let payload = b"hello";
        let message_id = 123;
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
