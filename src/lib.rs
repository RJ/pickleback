mod reliable;

use bytes::{Buf, Bytes, BytesMut};
use log::*;
use reliable::*;
use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    mem::take,
};
mod message;
use message::*;
// pub mod channel;
pub mod jitter_pipe;
mod message_reassembler;
mod test_utils;
use message_reassembler::*;
mod dispatcher;
use dispatcher::*;

#[derive(Debug)]
pub struct ReceivedMessage {
    pub channel: u8,
    pub payload: Bytes,
}

#[derive(Default)]
pub(crate) struct ChannelList {
    channels: HashMap<u8, Box<dyn Channel>>,
}
impl ChannelList {
    pub fn get_mut(&mut self, id: u8) -> Option<&mut Box<dyn Channel>> {
        self.channels.get_mut(&id)
    }
    fn put(&mut self, channel: Box<dyn Channel>) {
        self.channels.insert(channel.id(), channel);
    }
    fn channels_mut(
        &mut self,
    ) -> std::collections::hash_map::ValuesMut<'_, u8, std::boxed::Box<(dyn Channel + 'static)>>
    {
        self.channels.values_mut()
    }
    fn any_with_messages_to_send(&self) -> bool {
        for (_, channel) in self.channels.iter() {
            if !channel.is_empty() {
                return true;
            }
        }
        false
    }
    // fn non_empty_channels_mut(
    //     &mut self,
    // ) -> std::collections::hash_map::ValuesMut<'_, u8, std::boxed::Box<(dyn Channel + 'static)>>
    // {
    //     self.channels.values_mut()
    // }
}
trait Channel {
    fn id(&self) -> u8;
    fn update(&mut self, dt: f64);
    fn is_empty(&self) -> bool;
    /// enqueue a message to be sent in an outbound packet
    fn enqueue_message(&mut self, msg: Message);
    /// called when a message has been acked by the packet layer
    fn message_ack_received(&mut self, message_id: MessageId);
    /// get message ready to be coalesced into an outbound packet
    fn get_message_to_write_to_a_packet(&mut self, max_size: usize) -> Option<Message>;
}

#[derive(Default)]
struct UnreliableChannel {
    time: f64,
    id: u8,
    q: VecDeque<Message>,
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

pub struct Packeteer {
    endpoint: Endpoint,
    time: f64,
    dispatcher: MessageDispatcher,
    channels: ChannelList,
}

/// Represents one end of a datagram stream between two peers, one of which is the server.
///
/// ultimately probably want channels, IDed by a u8. then we can have per-channel settings.
/// eg ordering guarantees, reliability of messages, retransmit time, etc.
///
impl Packeteer {
    pub fn new(time: f64) -> Self {
        let endpoint_config = EndpointConfig {
            max_payload_size: 1024,
            ..Default::default()
        };
        let endpoint = Endpoint::new(endpoint_config, time);
        let mut channels = ChannelList::default();
        channels.put(Box::new(UnreliableChannel {
            id: 0,
            ..Default::default()
        }));
        Self {
            endpoint,
            time,
            dispatcher: MessageDispatcher::default(),
            channels,
        }
    }

    pub fn drain_packets_to_send(
        &mut self,
    ) -> std::collections::vec_deque::Drain<'_, bytes::Bytes> {
        self.write_packets_to_send();
        self.endpoint.drain_packets_to_send()
    }

    pub fn drain_received_messages(&mut self, channel: u8) -> std::vec::Drain<'_, ReceivedMessage> {
        self.dispatcher.drain_received_messages(channel)
    }

    pub fn drain_message_acks(&mut self, channel: u8) -> std::vec::Drain<'_, MessageId> {
        self.dispatcher.drain_message_acks(channel)
    }

    /// enqueue a message to be sent in a packet.
    /// messages get coalesced into packets.
    pub fn send_message(&mut self, channel: u8, message_payload: Bytes) -> MessageId {
        let channel = self.channels.get_mut(channel).expect("No such channel");
        self.dispatcher
            .add_message_to_channel(channel, message_payload)
    }

    // when creating the messages, we want one big BytesMut?? with views into it, refcounted so
    // once no more messages are alive, it's cleaned up? then we can do a large contiguous allocation
    // for lots of small message buffers..
    // otherwise it's fragmenty af
    // it's almost an arena allocator cleared per frame, but some messages might not be sent until next frame,
    // and reliables need to stick around even longer..
    //
    //
    fn write_packets_to_send(&mut self) {
        info!("write packets.");
        let mut sent_something = false;

        let mut message_handles_in_packet = Vec::<MessageHandle>::new();
        let max_packet_size = self.endpoint.config().max_packet_size;
        let mut packet = BytesMut::with_capacity(max_packet_size);
        let mut remaining_space = max_packet_size;
        // definitely scope to optimise these nested loops..
        // hopefully never sending too many packets per tick though, so maybe fine.
        while self.channels.any_with_messages_to_send() {
            info!("any with msg to send");
            // for all channels with messages to send:
            'non_empty_channels: while let Some(channel) =
                self.channels.channels_mut().find(|ch| !ch.is_empty())
            {
                let mut any_found = false;
                while let Some(msg) = channel.get_message_to_write_to_a_packet(remaining_space) {
                    any_found = true;
                    trace!("* Writing {msg:?} to packet buffer..");
                    msg.write(&mut packet)
                        .expect("writing to a buffer shouldn't fail");
                    message_handles_in_packet.push(MessageHandle {
                        id: msg.id(),
                        parent: msg.parent_id(),
                        channel: channel.id(),
                    });
                    remaining_space = max_packet_size - packet.len();
                    if remaining_space < 3 {
                        break 'non_empty_channels;
                    }
                }
                if !any_found {
                    break;
                }
            }
            if remaining_space == max_packet_size {
                continue;
            }
            // send packet.
            let final_packet = packet.freeze();
            packet = BytesMut::with_capacity(max_packet_size);
            remaining_space = max_packet_size;
            match self.endpoint.send(final_packet) {
                Ok(handle) => {
                    sent_something = true;
                    info!("Sending packet containing msg ids: {message_handles_in_packet:?}, in packet seq {handle:?}");
                    self.dispatcher
                        .set_packet_message_handles(handle, take(&mut message_handles_in_packet));
                }
                Err(err) => {
                    error!("Err sending coalesced packet {err:?}");
                }
            }
        }

        // if no Messages to send, we'll still send an empty-payload packet, so that
        // acks are transmitted.
        // sending one empty packet per tick is fine.. right? what about uncapped headless server?
        if !sent_something {
            info!("No msgs. Sending empty packet, jsut for acks");
            self.endpoint.send(Bytes::new()).unwrap();
        }
    }

    pub fn update(&mut self, dt: f64) {
        self.time += dt;
        self.endpoint.update(self.time);
        // updating time for channels may result in reliable channels enqueuing messages
        // that need to be retransmitted.
        for channel in self.channels.channels_mut() {
            channel.update(dt);
        }
    }

    /// a datagram was received, written to a Bytes, and passed in here for processing
    /// since we don't do the actual networking ourselves..
    ///
    /// could split this up by not returning ReceivedPacets, but writing to a queue
    /// so we could process incoming in PreUpdate, but only process the queue of received in Fixed?
    pub fn process_incoming_packet(&mut self, packet: Bytes) {
        match self.endpoint.receive(packet) {
            Ok(ReceivedPacket {
                handle: _,
                payload,
                acks,
            }) => {
                for acked_handle in &acks {
                    self.dispatcher
                        .acked_packet(acked_handle, &mut self.channels);
                }
                let mut reader = payload;
                while reader.remaining() > 0 {
                    match Message::parse(&mut reader) {
                        Ok(msg) => self.dispatcher.process_received_message(msg),
                        Err(err) => {
                            error!("Error parsing messages from packet payload: {err:?}");
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                warn!("incoming packet error {err:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::jitter_pipe::{JitterPipe, JitterPipeConfig};

    use super::*;
    // explicit import to override bevy
    // use log::{debug, error, info, trace, warn};

    #[test]
    fn small_unfrag_messages() {
        crate::test_utils::init_logger();
        let channel = 0;
        let mut server = Packeteer::new(1_f64);
        let msg1 = Bytes::from_static(b"Hello");
        let msg2 = Bytes::from_static(b"world");
        let msg3 = Bytes::from_static(b"!");
        server.send_message(channel, msg1.clone());
        server.send_message(channel, msg2.clone());
        server.send_message(channel, msg3.clone());

        let mut client = Packeteer::new(1_f64);
        // deliver msgs from server to client
        server
            .drain_packets_to_send()
            .for_each(|packet| client.process_incoming_packet(packet));

        let received_messages = client.drain_received_messages(channel).collect::<Vec<_>>();
        assert_eq!(received_messages[0].payload, msg1);
        assert_eq!(received_messages[1].payload, msg2);
        assert_eq!(received_messages[2].payload, msg3);

        // once client sends a message back to server, the acks will be send too
        client
            .drain_packets_to_send()
            .for_each(|packet| server.process_incoming_packet(packet));

        assert_eq!(
            vec![0, 1, 2],
            server.drain_message_acks(channel).collect::<Vec<_>>()
        );
    }

    #[test]
    fn frag_message() {
        crate::test_utils::init_logger();
        let channel = 0;
        let mut server = Packeteer::new(1_f64);
        let mut msg = BytesMut::new();
        msg.extend_from_slice(&[65; 1024]);
        msg.extend_from_slice(&[66; 1024]);
        msg.extend_from_slice(&[67; 100]);
        let msg = msg.freeze();

        let msg_id = server.send_message(channel, msg.clone());

        // server.write_packets_to_send();

        let mut client = Packeteer::new(1_f64);
        // deliver msgs from server to client
        server
            .drain_packets_to_send()
            .for_each(|packet| client.process_incoming_packet(packet));

        let received_messages = client.drain_received_messages(channel).collect::<Vec<_>>();
        assert_eq!(received_messages.len(), 1);
        assert_eq!(received_messages[0].payload, msg);

        // once client sends a message back to server, the acks will be sent too
        // client.write_packets_to_send();
        client
            .drain_packets_to_send()
            .for_each(|packet| server.process_incoming_packet(packet));

        assert_eq!(
            vec![msg_id],
            server.drain_message_acks(channel).collect::<Vec<_>>()
        );
    }

    fn random_payload(size: u32) -> Bytes {
        use bytes::BufMut;
        let mut b = BytesMut::with_capacity(size as usize);
        for _ in 0..size {
            b.put_u8(rand::random::<u8>());
        }
        b.freeze()
    }
    // TODO this isn't testing having multiple messages in flight yet? batch the sending and receiving?
    #[test]
    fn soak_message_transmission() {
        crate::test_utils::init_logger();
        let channel = 0;
        let mut server = Packeteer::new(1_f64);
        let mut client = Packeteer::new(1_f64);

        for _ in 0..10000 {
            let size = rand::random::<u32>() % (1024 * 16);
            let msg = random_payload(size);
            let msg_id = server.send_message(channel, msg.clone());
            println!("💌 Sending message of size {size}, msg_id: {msg_id}");
            server
                .drain_packets_to_send()
                .for_each(|packet| client.process_incoming_packet(packet));
            let received_messages = client.drain_received_messages(channel).collect::<Vec<_>>();
            assert_eq!(received_messages.len(), 1);
            assert_eq!(received_messages[0].payload, msg);

            assert!(server
                .drain_message_acks(channel)
                .collect::<Vec<_>>()
                .is_empty());

            client
                .drain_packets_to_send()
                .for_each(|packet| server.process_incoming_packet(packet));

            assert_eq!(
                vec![msg_id],
                server.drain_message_acks(channel).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn soak_message_transmission_with_jitter_pipe() {
        crate::test_utils::init_logger();
        let mut server = Packeteer::new(1_f64);
        let mut client = Packeteer::new(1_f64);

        let channel = 0;

        let mut server_jitter_pipe = JitterPipe::<Bytes>::new(JitterPipeConfig::disabled());
        let mut client_jitter_pipe = JitterPipe::<Bytes>::new(JitterPipeConfig::disabled());

        for _ in 0..10000 {
            let size = rand::random::<u32>() % (1024 * 16);
            let msg = random_payload(size);
            let msg_id = server.send_message(channel, msg.clone());
            println!("💌 Sending message of size {size}, msg_id: {msg_id}");

            server
                .drain_packets_to_send()
                .for_each(|packet| server_jitter_pipe.insert(packet));
            while let Some(p) = server_jitter_pipe.take_next() {
                client.process_incoming_packet(p);
            }

            let received_messages = client.drain_received_messages(channel).collect::<Vec<_>>();
            assert_eq!(received_messages.len(), 1);
            assert_eq!(received_messages[0].payload, msg);

            assert!(server
                .drain_message_acks(channel)
                .collect::<Vec<_>>()
                .is_empty());

            client
                .drain_packets_to_send()
                .for_each(|packet| client_jitter_pipe.insert(packet));
            while let Some(p) = client_jitter_pipe.take_next() {
                server.process_incoming_packet(p);
            }

            assert_eq!(
                vec![msg_id],
                server.drain_message_acks(channel).collect::<Vec<_>>()
            );
        }
    }
}
