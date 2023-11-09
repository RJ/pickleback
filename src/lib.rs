mod reliable;

// use bytes::{BufMut, BytesMut};
use std::io::{Cursor, Write};
use std::{collections::VecDeque, net::SocketAddr};
// use anyhow::Result;
use bevy::prelude::*;
use bevy::utils::HashMap;
use byteorder::{BigEndian, WriteBytesExt};
use bytes::{BufMut, Bytes, BytesMut};
use reliable::*;

// pub struct Datagram {
//     pub payload: Vec<u8>,
//     pub address: SocketAddr,
// }

pub type MessageId = u32;

#[derive(Debug)]
struct UnreliableMessage {
    id: MessageId,
    payload: Bytes,
}
impl UnreliableMessage {
    fn size(&self) -> usize {
        self.payload.len()
    }
    fn payload(&self) -> &Bytes {
        &self.payload
    }
}

#[derive(Default)]
struct UnreliableMessageOutbox {
    q: VecDeque<UnreliableMessage>,
    messages_in_packets: HashMap<SentHandle, Vec<MessageId>>,
    message_seq: MessageId,
}
impl UnreliableMessageOutbox {
    fn add(&mut self, payload: Bytes) -> MessageId {
        let id = self.message_seq;
        self.message_seq += 1;
        let msg = UnreliableMessage { id, payload };
        self.q.push_back(msg);
        id
    }
    fn is_empty(&self) -> bool {
        self.q.is_empty()
    }
    /// removes and returns oldest message from queue within max_size constraint
    fn take_message(&mut self, max_size: usize) -> Option<UnreliableMessage> {
        for index in 0..self.q.len() {
            if self.q[index].size() > max_size {
                continue;
            }
            return self.q.remove(index);
        }
        None
    }
    fn register_message_ids_in_packet(&mut self, handle: SentHandle, message_ids: Vec<MessageId>) {
        self.messages_in_packets.insert(handle, message_ids);
    }
    fn take_message_ids_for_handle(&mut self, handle: &SentHandle) -> Option<Vec<MessageId>> {
        self.messages_in_packets.remove(handle)
    }
}

pub struct Servent {
    endpoint: Endpoint,
    time: f64,
    /// messages waiting to be coalesced into packets for sending
    unreliable_message_outbox: UnreliableMessageOutbox,
    // received_packet_inbox:
    // reliable_message_outbox: VecDeque<(u32, &'s [u8])>,
}
const MAX_PAYLOAD_SIZE: u16 = 1150;

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
            max_packet_size: (16 * MAX_PAYLOAD_SIZE).into(),
            fragment_above: MAX_PAYLOAD_SIZE.into(),
            max_fragments: 16,
            ..default()
        };
        let endpoint = Endpoint::new(endpoint_config, time);
        Self {
            endpoint,
            time,
            unreliable_message_outbox: UnreliableMessageOutbox::default(),
        }
    }

    pub fn allocate_packet_buffer(&mut self) -> Box<[u8]> {
        let packet_max_size = self.endpoint.config().fragment_above;
        vec![0; packet_max_size].into_boxed_slice()
    }

    /// enqueue a message to be sent in a packet.
    /// messages get coalesced into packets.
    pub fn send_message(&mut self, message_payload: Bytes) -> MessageId {
        self.unreliable_message_outbox.add(message_payload)
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
        let fragment_size = self.endpoint.config().fragment_above;
        let mut cursor = Cursor::new(self.allocate_packet_buffer());
        // let mut buf = BytesMut::with_capacity(fragment_size);

        // TODO always include all unacked reliable msgs at the start of the packet,
        // provided they haven't been sent in the last 0.1 seconds.

        // what about messages that required packet fragments?

        // we need to create packets from the messages.
        // lots of small unreliable messages can be coalesced into a packet.
        // small reliable msgs can be added first, to those packets, if unacked within 100ms.

        // first pass: send unreliable messages > fragment size as fragmented packets.
        // second pass: coalesce remaining unreliable messages until outbox is empty.

        while !self.unreliable_message_outbox.is_empty() {
            let mut message_ids_in_packet = Vec::<MessageId>::new();
            let mut packet = BytesMut::new();

            while let Some(msg) = self
                .unreliable_message_outbox
                .take_message(fragment_size - 2 as usize)
            {
                if packet.len() == 0 {
                    if msg.size() <= self.endpoint.config().fragment_above {
                        packet.reserve();
                    }
                }
                info!("* Writing {msg:?} to packet buffer..");
                cursor.write_u16::<BigEndian>(msg.size() as u16).unwrap();
                cursor.write_all(msg.payload().as_ref()).unwrap();
                message_ids_in_packet.push(msg.id);
            }

            let packet_size = cursor.position() as usize;
            let buffer = cursor.into_inner();
            let packet_payload = &(buffer.as_ref())[0..packet_size];

            match self.endpoint.send(Bytes::from(packet_payload)) {
                Ok(handle) => {
                    info!("Sending packet containing msg ids: {message_ids_in_packet:?}, in packet seq {handle:?}");
                    self.unreliable_message_outbox
                        .register_message_ids_in_packet(handle, message_ids_in_packet);
                }
                Err(err) => {
                    error!("Err sending coalesced packet {err:?}");
                }
            }
        }
    }

    /// called after acks and headers stripped, we just process the payload
    /// do we plug in some sort of packet state machine object here, or what?
    fn process_received_packet(&mut self, packet: ReceivedPacket) {
        // possibly just write to an inbox?
        info!("process_received_packet: {packet:?}");
    }

    // this needs to ack message ids assocated with the handle
    fn ack_packet_handle(&mut self, handle: &SentHandle) {
        info!("ACK packet handle {handle:?}");
        if let Some(msg_ids) = self
            .unreliable_message_outbox
            .take_message_ids_for_handle(handle)
        {
            for msg_id in msg_ids {
                info!("* Ack message {msg_id} via packet {handle:?}");
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
        match self.endpoint.recv(packet) {
            Ok((opt_packet, new_acks)) => {
                for handle in &new_acks {
                    self.ack_packet_handle(handle);
                }
                if let Some(packet) = opt_packet {
                    self.process_received_packet(packet);
                }
            }
            Err(err) => {
                warn!("incoming packet error {err:?}");
            }
        }
    }
}
