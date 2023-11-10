mod reliable;

use bytes::{Buf, Bytes, BytesMut};
use jitter_pipe::JitterPipeConfig;
use log::*;
use reliable::*;
use std::collections::{hash_map::Entry, HashMap, VecDeque};
mod message;
use message::*;
pub mod jitter_pipe;
mod test_utils;

/// You get a MessageHandle when you call `send_message(payload)`.
/// It's only useful if you want to know if an ack was received for this message.
#[derive(Default, Debug)]
pub struct MessageHandle {
    id: MessageId,
    parent: Option<MessageId>,
}
impl MessageHandle {
    pub fn new(id: MessageId) -> Self {
        Self { id, parent: None }
    }
    pub fn id(&self) -> MessageId {
        self.id
    }
    pub fn is_fragment(&self) -> bool {
        self.parent.is_some()
    }

    pub fn parent_id(&self) -> Option<MessageId> {
        self.parent
    }
}

#[derive(Debug)]
pub struct ReceivedMessage {
    pub channel: u8,
    pub payload: Bytes,
}

/// tracks the unacked message ids assigned to fragments of a larger message id.
/// they're removed as they are acked & once depleted, the original parent message id is acked.
#[derive(Default)]
pub struct SentFragMap {
    m: HashMap<MessageId, Vec<MessageId>>,
}
impl SentFragMap {
    pub fn insert_fragmented_message(&mut self, id: MessageId, fragment_ids: Vec<MessageId>) {
        let res = self.m.insert(id, fragment_ids);
        assert!(
            res.is_none(),
            "why are we overwriting something in the fragmap?"
        );
    }
    /// returns true if parent message is whole/acked.
    pub fn ack_message_id_for_fragment(
        &mut self,
        parent_message_id: MessageId,
        id_to_ack: MessageId,
    ) -> bool {
        let ret = if let Some(v) = self.m.get_mut(&parent_message_id) {
            v.retain(|id| *id != id_to_ack);
            v.is_empty()
        } else {
            false
        };
        info!(
            "unacked frags left for parent: {parent_message_id} = {:?} ",
            self.m.get(&parent_message_id).unwrap()
        );
        if ret {
            self.m.remove(&parent_message_id);
        }
        ret
    }
}

#[derive(Default)]
struct MessageDispatcher {
    // q_reliable: VecDeque<Message>,
    q_unreliable: VecDeque<Message>,
    next_message_id: MessageId,
    sent_frag_map: SentFragMap,
    next_frag_group_id: u8,
    messages_in_packets: HashMap<SentHandle, Vec<MessageHandle>>,
}
impl MessageDispatcher {
    /// sets the list of messageids contained in the packet
    fn set_packet_message_handles(
        &mut self,
        packet_handle: SentHandle,
        message_handles: Vec<MessageHandle>,
    ) {
        self.messages_in_packets
            .insert(packet_handle, message_handles);
    }
    // got a packet ack? we'll give you message ids that it acks.
    fn acked_packet_to_acked_messages(&mut self, packet_handle: &SentHandle) -> Vec<MessageId> {
        let mut acked_ids = Vec::new();
        // check message handles that were just acked - if any are fragments, we need to log that
        // in the frag map, incase it results in a parent message id being acked (ie, all frag messages are now acked)
        if let Some(msg_handles) = self.messages_in_packets.remove(packet_handle) {
            for msg_handle in &msg_handles {
                if let Some(parent_id) = msg_handle.parent_id() {
                    info!("got ack for {msg_handle:?}, removing from fragmap");
                    // fragment message
                    if self
                        .sent_frag_map
                        .ack_message_id_for_fragment(parent_id, msg_handle.id())
                    {
                        info!("all frags acked, acking parent msg id {parent_id}");
                        acked_ids.push(parent_id);
                    }
                } else {
                    acked_ids.push(msg_handle.id());
                }
            }
        }
        acked_ids
    }

    fn next_frag_group_id(&mut self) -> u8 {
        let ret = self.next_frag_group_id;
        self.next_frag_group_id = self.next_frag_group_id.wrapping_add(1);
        ret
    }

    fn next_message_id(&mut self) -> MessageId {
        let ret = self.next_message_id;
        self.next_message_id = self.next_message_id.wrapping_add(1);
        ret
    }

    /// takes a payload of any size and writes messages to the outbox queues.
    /// will fragment into multiple messages if payload is > 1024
    fn add(&mut self, payload: Bytes) -> MessageId {
        if payload.len() <= 1024 {
            self.add_unfragmented(payload)
        } else {
            self.add_fragmented(payload)
        }
    }

    fn add_unfragmented(&mut self, payload: Bytes) -> MessageId {
        assert!(payload.len() <= 1024);
        let id = self.next_message_id();
        let channel = 0;
        let msg = Message::new_unfragmented(id, channel, payload);
        // put on reliable or unreliable q based on channel?
        self.q_unreliable.push_back(msg);
        id
    }

    fn add_fragmented(&mut self, mut payload: Bytes) -> MessageId {
        assert!(payload.len() > 1024);
        // this is the message id we return to the user, upon which they await an ack.
        let parent_id = self.next_message_id();
        let payload_len = payload.len();
        let channel = 0;
        let fragment_group_id = self.next_frag_group_id();
        // split into multiple messages.
        // track fragmented part message ids associated with the parent message ID which we return
        // so we can ack it once all fragment messages are acked.
        let remainder = if payload_len % 1024 > 0 { 1 } else { 0 };
        let num_fragments = (payload_len / 1024) + remainder;
        let mut fragment_ids = Vec::new();
        for fragment_id in 0..num_fragments {
            let payload_size = if fragment_id == num_fragments - 1 {
                payload_len - (num_fragments - 1) * 1024
            } else {
                1024
            };
            let frag_payload = payload.split_to(payload_size);
            let fragment_message_id = self.next_message_id();
            info!("Adding frag msg parent:{parent_id} child:{fragment_message_id}  frag:{fragment_id}/{num_fragments}");
            let msg = Message::new_fragment(
                fragment_message_id,
                channel,
                frag_payload,
                fragment_group_id,
                fragment_id as u16,
                num_fragments as u16,
                parent_id,
            );
            fragment_ids.push(fragment_message_id);
            // put on reliable or unreliable q based on channel?
            self.q_unreliable.push_back(msg);
        }
        self.sent_frag_map
            .insert_fragmented_message(parent_id, fragment_ids);

        parent_id
    }

    fn is_empty(&self) -> bool {
        self.q_unreliable.is_empty()
    }
    /// removes and returns oldest message from queue within max_size constraint
    fn take_unreliable_message(&mut self, max_size: usize) -> Option<Message> {
        for index in 0..self.q_unreliable.len() {
            if self.q_unreliable[index].size() > max_size {
                continue;
            }
            return self.q_unreliable.remove(index);
        }
        None
    }
}

/// max fragments set to 1024 for now.
/// could be a dynamically size vec based on num-fragments tho..
struct IncompleteMessage {
    channel: u8,
    num_fragments: u16,
    num_received_fragments: u16,
    fragments: [Option<Bytes>; 1024],
}

impl IncompleteMessage {
    fn new(channel: u8, num_fragments: u16) -> Self {
        Self {
            channel,
            num_fragments,
            num_received_fragments: 0,
            fragments: std::array::from_fn(|_| None),
        }
    }
    fn add_fragment(&mut self, fragment_id: u16, payload: Bytes) -> Option<ReceivedMessage> {
        assert!(fragment_id < 1024);
        if self.fragments[fragment_id as usize].is_none() {
            self.fragments[fragment_id as usize] = Some(payload);
            self.num_received_fragments += 1;
            if self.num_received_fragments == self.num_fragments {
                let size = (self.num_fragments as usize - 1) * 1024
                    + self.fragments[self.num_fragments as usize - 1]
                        .as_ref()
                        .unwrap()
                        .len();
                let mut reassembled_payload = BytesMut::with_capacity(size);
                for i in 0..self.num_fragments {
                    reassembled_payload
                        .extend_from_slice(self.fragments[i as usize].as_ref().unwrap());
                }
                return Some(ReceivedMessage {
                    channel: self.channel,
                    payload: reassembled_payload.freeze(),
                });
            }
        }
        None
    }
}

#[derive(Default)]
struct MessageReassembler {
    in_progress: HashMap<FragGroupId, IncompleteMessage>,
}

impl MessageReassembler {
    fn add_fragment(&mut self, message: &Message) -> Option<ReceivedMessage> {
        let Some(fragment) = message.fragment() else {
            return Some(ReceivedMessage {
                channel: message.channel(),
                payload: message.payload().clone(),
            });
        };

        let ret = match self.in_progress.entry(fragment.group_id) {
            Entry::Occupied(entry) => entry
                .into_mut()
                .add_fragment(fragment.id, message.payload().clone()),
            Entry::Vacant(v) => {
                let incomp_msg = v.insert(IncompleteMessage::new(
                    message.channel(),
                    fragment.num_fragments,
                ));
                incomp_msg.add_fragment(fragment.id, message.payload().clone())
            }
        };

        if ret.is_some() {
            // resulted in a fully reassembled message, cleanup:
            info!("Reassembly complete for {fragment:?}");
            self.in_progress.remove(&fragment.group_id);
        }

        ret
    }
}

pub struct Servent {
    endpoint: Endpoint,
    time: f64,
    // messages parsed out of packets we received. channel and payload.
    message_inbox: VecDeque<ReceivedMessage>,
    /// messages waiting to be coalesced into packets for sending
    message_dispatcher: MessageDispatcher,

    message_ack_inbox: VecDeque<MessageId>,

    message_reassembler: MessageReassembler,
    // received_packet_inbox:
    // reliable_message_outbox: VecDeque<(u32, &'s [u8])>,
}

/// Represents one end of a datagram stream between two peers, one of which is the server.
///
/// ultimately probably want channels, IDed by a u8. then we can have per-channel settings.
/// eg ordering guarantees, reliability of messages, retransmit time, etc.
///
impl Servent {
    pub fn new(time: f64) -> Self {
        let endpoint_config = EndpointConfig {
            max_payload_size: 1024,
            ..Default::default()
        };
        let endpoint = Endpoint::new(endpoint_config, time);
        Self {
            endpoint,
            time,
            message_dispatcher: MessageDispatcher::default(),
            message_inbox: VecDeque::new(),
            message_ack_inbox: VecDeque::new(),
            message_reassembler: MessageReassembler::default(),
        }
    }

    pub fn drain_packets_to_send(
        &mut self,
    ) -> std::collections::vec_deque::Drain<'_, bytes::Bytes> {
        self.endpoint.drain_packets_to_send()
    }

    pub fn drain_received_messages(
        &mut self,
    ) -> std::collections::vec_deque::Drain<'_, ReceivedMessage> {
        self.message_inbox.drain(..)
    }

    pub fn drain_message_acks(&mut self) -> std::collections::vec_deque::Drain<'_, MessageId> {
        self.message_ack_inbox.drain(..)
    }

    /// enqueue a message to be sent in a packet.
    /// messages get coalesced into packets.
    pub fn send_message(&mut self, message_payload: Bytes) -> MessageId {
        self.message_dispatcher.add(message_payload)
    }

    // when creating the messages, we want one big BytesMut?? with views into it, refcounted so
    // once no more messages are alive, it's cleaned up? then we can do a large contiguous allocation
    // for lots of small message buffers..
    // otherwise it's fragmenty af
    // it's almost an arena allocator cleared per frame, but some messages might not be sent until next frame,
    // and reliables need to stick around even longer..
    //
    //
    pub fn write_packets_to_send(&mut self) {
        // if no Messages to send, we'll still send an empty-payload packet, so that
        // acks are transmitted.
        // sending one empty packet per tick is fine.. right? what about uncapped headless server?
        if self.message_dispatcher.is_empty() {
            info!("No msgs. Sending empty packet, jsut for acks");
            self.endpoint.send(Bytes::new()).unwrap();
            return;
        }
        // message ids aren't sent over the wire, the remote end doesn't care.
        // they are only used locally for tracking acks. we map packet-level acks back to
        // message ids, so we report messages acks to the consumer.
        // we just need to track which messages ids were in which packet sequence number.
        while !self.message_dispatcher.is_empty() {
            let mut message_handles_in_packet = Vec::<MessageHandle>::new();
            trace!("Allocated new packet to coalesce into.");
            // max packet size something like 1150 bytes
            let max_packet_size = self.endpoint.config().max_packet_size;
            let mut packet = BytesMut::with_capacity(max_packet_size);
            let mut remaining_space = max_packet_size;
            while let Some(msg) = self
                .message_dispatcher
                .take_unreliable_message(remaining_space)
            {
                trace!("* Writing {msg:?} to packet buffer..");
                msg.write(&mut packet)
                    .expect("writing to a buffer shouldn't fail");
                message_handles_in_packet.push(MessageHandle {
                    id: msg.id(),
                    parent: msg.parent_id(),
                });
                remaining_space = max_packet_size - packet.len();
            }
            // now send this packet (ie, enqueue it in endpoint's outbox)

            match self.endpoint.send(packet.freeze()) {
                Ok(handle) => {
                    info!("Sending packet containing msg ids: {message_handles_in_packet:?}, in packet seq {handle:?}");
                    self.message_dispatcher
                        .set_packet_message_handles(handle, message_handles_in_packet);
                }
                Err(err) => {
                    error!("Err sending coalesced packet {err:?}");
                }
            }
        }
    }

    /// called after acks and headers stripped, we just process the payload
    /// by parsing out the messages and yielding them to consumer.
    fn read_messages_from_received_packet_payload(&mut self, mut payload: Bytes) {
        // possibly just write to an inbox?
        info!("process_received_packet, len: {}", payload.len());
        while payload.remaining() > 0 {
            match Message::parse(&mut payload) {
                Ok(msg) => {
                    // in case it's a fragment, do reassembly.
                    // this just yields a ReceivedMessage instantly for non-fragments:
                    if let Some(received_msg) = self.message_reassembler.add_fragment(&msg) {
                        info!(
                            "received complete message, len: {}",
                            received_msg.payload.len()
                        );
                        self.message_inbox.push_back(received_msg);
                    }
                }
                Err(err) => {
                    error!("Error parsing messages from packet payload: {err:?}");
                    break;
                }
            }
        }
    }

    pub fn update(&mut self, dt: f64) {
        self.time += dt;
        self.endpoint.update(self.time);
        // potentially enqueue retransmits of unacked reliables, or send heartbeats
        // timeout clients who haven't sent packets in a while
        // etc.
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
                    let msg_acks = self
                        .message_dispatcher
                        .acked_packet_to_acked_messages(acked_handle);
                    if !msg_acks.is_empty() {
                        info!("NEW MSG ACKS (via packet ack of {acked_handle:?} = {msg_acks:?}");
                        self.message_ack_inbox.extend(msg_acks.iter());
                    }
                }
                self.read_messages_from_received_packet_payload(payload);
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
        let mut server = Servent::new(1_f64);
        let msg1 = Bytes::from_static(b"Hello");
        let msg2 = Bytes::from_static(b"world");
        let msg3 = Bytes::from_static(b"!");
        server.send_message(msg1.clone());
        server.send_message(msg2.clone());
        server.send_message(msg3.clone());

        server.write_packets_to_send();

        let mut client = Servent::new(1_f64);
        // deliver msgs from server to client
        server
            .drain_packets_to_send()
            .for_each(|packet| client.process_incoming_packet(packet));

        let received_messages = client.drain_received_messages().collect::<Vec<_>>();
        assert_eq!(received_messages[0].payload, msg1);
        assert_eq!(received_messages[1].payload, msg2);
        assert_eq!(received_messages[2].payload, msg3);

        // once client sends a message back to server, the acks will be send too
        client.write_packets_to_send();
        client
            .drain_packets_to_send()
            .for_each(|packet| server.process_incoming_packet(packet));

        assert_eq!(
            vec![0, 1, 2],
            server.drain_message_acks().collect::<Vec<_>>()
        );
    }

    #[test]
    fn frag_message() {
        crate::test_utils::init_logger();
        let mut server = Servent::new(1_f64);
        let mut msg = BytesMut::new();
        msg.extend_from_slice(&[65; 1024]);
        msg.extend_from_slice(&[66; 1024]);
        msg.extend_from_slice(&[67; 100]);
        let msg = msg.freeze();

        let msg_id = server.send_message(msg.clone());

        server.write_packets_to_send();

        let mut client = Servent::new(1_f64);
        // deliver msgs from server to client
        server
            .drain_packets_to_send()
            .for_each(|packet| client.process_incoming_packet(packet));

        let received_messages = client.drain_received_messages().collect::<Vec<_>>();
        assert_eq!(received_messages.len(), 1);
        assert_eq!(received_messages[0].payload, msg);

        // once client sends a message back to server, the acks will be sent too
        client.write_packets_to_send();
        client
            .drain_packets_to_send()
            .for_each(|packet| server.process_incoming_packet(packet));

        assert_eq!(
            vec![msg_id],
            server.drain_message_acks().collect::<Vec<_>>()
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
        let mut server = Servent::new(1_f64);
        let mut client = Servent::new(1_f64);

        for _ in 0..10000 {
            let size = rand::random::<u32>() % (1024 * 16);
            let msg = random_payload(size);
            let msg_id = server.send_message(msg.clone());
            println!("💌 Sending message of size {size}, msg_id: {msg_id}");
            server.write_packets_to_send();
            server
                .drain_packets_to_send()
                .for_each(|packet| client.process_incoming_packet(packet));
            let received_messages = client.drain_received_messages().collect::<Vec<_>>();
            assert_eq!(received_messages.len(), 1);
            assert_eq!(received_messages[0].payload, msg);

            assert!(server.drain_message_acks().collect::<Vec<_>>().is_empty());

            client.write_packets_to_send();
            client
                .drain_packets_to_send()
                .for_each(|packet| server.process_incoming_packet(packet));

            assert_eq!(
                vec![msg_id],
                server.drain_message_acks().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn soak_message_transmission_with_jitter_pipe() {
        crate::test_utils::init_logger();
        let mut server = Servent::new(1_f64);
        let mut client = Servent::new(1_f64);

        let mut server_jitter_pipe = JitterPipe::<Bytes>::new(JitterPipeConfig::disabled());
        let mut client_jitter_pipe = JitterPipe::<Bytes>::new(JitterPipeConfig::disabled());

        for _ in 0..10000 {
            let size = rand::random::<u32>() % (1024 * 16);
            let msg = random_payload(size);
            let msg_id = server.send_message(msg.clone());
            println!("💌 Sending message of size {size}, msg_id: {msg_id}");
            server.write_packets_to_send();

            server
                .drain_packets_to_send()
                .for_each(|packet| server_jitter_pipe.insert(packet));
            while let Some(p) = server_jitter_pipe.take_next() {
                client.process_incoming_packet(p);
            }

            let received_messages = client.drain_received_messages().collect::<Vec<_>>();
            assert_eq!(received_messages.len(), 1);
            assert_eq!(received_messages[0].payload, msg);

            assert!(server.drain_message_acks().collect::<Vec<_>>().is_empty());

            client.write_packets_to_send();
            client
                .drain_packets_to_send()
                .for_each(|packet| client_jitter_pipe.insert(packet));
            while let Some(p) = client_jitter_pipe.take_next() {
                server.process_incoming_packet(p);
            }

            assert_eq!(
                vec![msg_id],
                server.drain_message_acks().collect::<Vec<_>>()
            );
        }
    }
}
