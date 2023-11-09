mod reliable;

// use bytes::{BufMut, BytesMut};
use std::io::{Cursor, Write};
use std::{collections::VecDeque, net::SocketAddr};
// use anyhow::Result;
use bevy::prelude::*;
use bevy::utils::HashMap;
use byteorder::{BigEndian, WriteBytesExt};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use reliable::*;
mod message;
use message::*;

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
        let id = self.next_message_id;
        self.next_message_id += 1;
        let channel = 0;
        let msg = Message::new_unfragmented(id, channel, payload);
        // put on reliable or unreliable q based on channel?
        self.q_unreliable.push_back(msg);
        id
    }

    fn add_fragmented(&mut self, mut payload: Bytes) -> MessageId {
        assert!(payload.len() > 1024);
        // this is the message id we return to the user, upon which they await an ack.
        let parent_id = self.next_message_id;
        self.next_message_id += 1;
        let payload_len = payload.len();
        let channel = 0;

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
            let fragment_id = self.next_message_id;
            self.next_message_id += 1;
            let msg = Message::new_fragment(
                fragment_id,
                channel,
                frag_payload,
                fragment_id as u16,
                num_fragments as u16,
                parent_id,
            );
            fragment_ids.push(fragment_id);
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
            if max_size > 0 && self.q_unreliable[index].size() > max_size {
                continue;
            }
            return self.q_unreliable.remove(index);
        }
        None
    }
}

pub struct Servent {
    endpoint: Endpoint,
    time: f64,
    /// messages waiting to be coalesced into packets for sending
    message_dispatcher: MessageDispatcher,
    // received_packet_inbox:
    // reliable_message_outbox: VecDeque<(u32, &'s [u8])>,
}

/// Represents one end of a datagram stream between two peers, one of which is the server.
///
///      Servent (server) <---datagrams---> Servent (client)
///
/// ultimately probably want channels, IDed by a u8. then we can have per-channel settings.
/// eg ordering guarantees, reliability of messages, retransmit time, etc.
///
impl Servent {
    pub fn new(time: f64) -> Self {
        let endpoint_config = EndpointConfig {
            max_payload_size: 1024,
            ..default()
        };
        let endpoint = Endpoint::new(endpoint_config, time);
        Self {
            endpoint,
            time,
            message_dispatcher: MessageDispatcher::default(),
        }
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
    pub fn coalesce_and_send_messages(&mut self) {
        // message ids aren't sent over the wire, the remote end doesn't care.
        // they are only used locally for tracking acks. we map packet-level acks back to
        // message ids, so we report messages acks to the consumer.
        // we just need to track which messages ids were in which packet sequence number.
        while !self.message_dispatcher.is_empty() {
            let mut message_handles_in_packet = Vec::<MessageHandle>::new();
            info!("Allocated new packet to coalesce into.");
            let mut packet = BytesMut::with_capacity(1150); // 1024 + headers etc.. should calculate exactly.

            while let Some(msg) = self
                .message_dispatcher
                .take_unreliable_message(packet.remaining())
            {
                info!("* Writing {msg:?} to packet buffer..");
                msg.write(&mut packet)
                    .expect("writing to a buffer shouldn't fail");
                message_handles_in_packet.push(MessageHandle {
                    id: msg.id(),
                    parent: msg.parent_id(),
                });
            }
            // now send this packet:

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
    /// do we plug in some sort of packet state machine object here, or what?
    fn process_received_packet(&mut self, payload: Bytes) {
        // possibly just write to an inbox?
        info!("process_received_packet: {payload:?}");
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
                    info!("NEW MSG ACKS (via packet ack of {acked_handle:?} = {msg_acks:?}");
                }
                self.process_received_packet(payload);
            }
            Err(err) => {
                warn!("incoming packet error {err:?}");
            }
        }
    }
}
