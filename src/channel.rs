use crate::*;
use std::time::{Duration, Instant};

// resend time could be dynamic based on recent rtt calculations per client?

#[derive(Debug, Clone)]
pub struct ChannelConfig {
    id: u8,
    reliable: bool,
    ordered: bool,
    resend_time: Option<Duration>,
}

pub enum DefaultChannels {
    Unreliable,
    Reliable,
}

impl From<DefaultChannels> for u8 {
    fn from(value: DefaultChannels) -> Self {
        match value {
            DefaultChannels::Unreliable => 0,
            DefaultChannels::Reliable => 1,
        }
    }
}

// in lieu of a HashMap<u8,T>, since we have a fixed small number of channel ids
pub(crate) struct ChannelIdMap<T>([Option<T>; 64]);
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

impl DefaultChannels {
    pub fn channel_config() -> Vec<ChannelConfig> {
        vec![
            ChannelConfig {
                id: 0,
                reliable: false,
                ordered: false,
                resend_time: None,
            },
            ChannelConfig {
                id: 1,
                reliable: true,
                ordered: false,
                resend_time: Some(Duration::from_millis(100)),
            },
        ]
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
        self.channels.all_mut().filter(|c| !c.is_empty())
    }
    pub(crate) fn any_with_messages_to_send(&self) -> bool {
        for ch in self.channels.all() {
            if !ch.is_empty() {
                return true;
            }
        }
        false
    }
}
pub(crate) trait Channel {
    fn id(&self) -> u8;
    fn update(&mut self, time: f64);
    fn is_empty(&self) -> bool;
    /// enqueue a message to be sent in an outbound packet
    fn enqueue_message(&mut self, msg: Message);
    /// called when a message has been acked by the packet layer
    fn message_ack_received(&mut self, message_id: MessageId);
    /// get message ready to be coalesced into an outbound packet
    fn get_message_to_write_to_a_packet(&mut self, max_size: usize) -> Option<Message>;
}

pub(crate) struct UnreliableChannel {
    time: f64,
    pub(crate) id: u8,
    q: VecDeque<Message>,
}

impl UnreliableChannel {
    pub(crate) fn new(id: u8, time: f64) -> Self {
        Self {
            id,
            time,
            q: VecDeque::default(),
        }
    }
}

impl Channel for UnreliableChannel {
    fn update(&mut self, dt: f64) {
        self.time += dt;
    }
    fn enqueue_message(&mut self, msg: Message) {
        assert_eq!(msg.channel(), self.id());
        self.q.push_back(msg);
    }
    fn is_empty(&self) -> bool {
        self.q.is_empty()
    }
    fn id(&self) -> u8 {
        self.id
    }
    fn message_ack_received(&mut self, _: MessageId) {
        // we don't care for unreliable channels?
    }
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
    last_sent: f64,
}
impl ResendableMessage {
    fn send_if_ready(&mut self, now: f64, cutoff: f64) -> bool {
        if self.last_sent == 0.0 || (now - self.last_sent) >= cutoff {
            self.last_sent = now;
            return true;
        }
        false
    }
}

pub(crate) struct ReliableChannel {
    time: f64,
    pub(crate) id: u8,
    q: VecDeque<ResendableMessage>,
}

impl ReliableChannel {
    pub(crate) fn new(id: u8, time: f64) -> Self {
        Self {
            id,
            time,
            q: VecDeque::default(),
        }
    }
}

impl Channel for ReliableChannel {
    fn update(&mut self, dt: f64) {
        self.time += dt;
    }
    fn enqueue_message(&mut self, message: Message) {
        assert_eq!(message.channel(), self.id());
        self.q.push_back(ResendableMessage {
            message,
            last_sent: 0.0,
        });
    }
    fn is_empty(&self) -> bool {
        self.q.is_empty()
    }
    fn id(&self) -> u8 {
        self.id
    }
    fn message_ack_received(&mut self, _: MessageId) {
        // we don't care for unreliable channels?
    }
    // this removes after returning, but a reliable queue shouldn't until acked.
    fn get_message_to_write_to_a_packet(&mut self, max_size: usize) -> Option<Message> {
        for index in 0..self.q.len() {
            let re_msg = self.q.get_mut(index).unwrap();
            if re_msg.message.size() > max_size {
                continue;
            }
            if re_msg.send_if_ready(self.time, 0.1) {
                return Some(re_msg.message.clone());
            }
        }
        None
    }
}
