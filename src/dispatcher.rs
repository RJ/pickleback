use crate::*;

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct MessageHandle {
    pub(crate) id: MessageId,
    pub(crate) frag_index: Option<u16>,
    pub(crate) channel: u8,
}
impl MessageHandle {
    pub fn id(&self) -> MessageId {
        self.id
    }
    pub fn parent_id(&self) -> Option<MessageId> {
        self.frag_index
            .map(|frag_index| MessageId(self.id.0.wrapping_sub(frag_index)))
    }
}

pub(crate) struct MessageDispatcher {
    next_message_id: u16,
    sent_frag_map: SentFragMap,
    messages_in_packets: SequenceBuffer<Vec<MessageHandle>>,
    message_reassembler: MessageReassembler,
    message_inbox: smallmap::Map<u8, Vec<ReceivedMessage>>,
    ack_inbox: smallmap::Map<u8, Vec<MessageId>>,
}

impl MessageDispatcher {
    pub(crate) fn new(config: &PacketeerConfig) -> Self {
        Self {
            next_message_id: 0,
            sent_frag_map: SentFragMap::with_capacity(config.sent_frag_map_size),
            messages_in_packets: SequenceBuffer::with_capacity(config.received_packets_buffer_size),
            message_reassembler: MessageReassembler::default(),
            message_inbox: smallmap::Map::default(),
            ack_inbox: smallmap::Map::default(),
        }
    }
    // pass in msgs parsed from received network packets.
    pub(crate) fn process_received_message(&mut self, msg: Message) {
        trace!("Dispatcher::process_received_message: {msg:?}");
        let received_msg = if msg.fragment().is_none() {
            Some(ReceivedMessage::new_single(msg))
        } else {
            self.message_reassembler.add_fragment(msg)
        };

        if let Some(msg) = received_msg {
            info!("✅ Adding msg to inbox");
            self.message_inbox
                .entry(msg.channel())
                .or_default()
                .push(msg);
        }
    }

    /// get the final messages for the consumer
    pub(crate) fn drain_received_messages(
        &mut self,
        channel: u8,
    ) -> std::vec::Drain<'_, ReceivedMessage> {
        self.message_inbox.entry(channel).or_default().drain(..)
    }

    /// get the acked  messages for the consumer
    pub(crate) fn drain_message_acks(&mut self, channel: u8) -> std::vec::Drain<'_, MessageId> {
        self.ack_inbox.entry(channel).or_default().drain(..)
    }

    /// sets the list of messageids contained in the packet
    pub(crate) fn set_packet_message_handles(
        &mut self,
        packet_handle: PacketId,
        message_handles: Vec<MessageHandle>,
    ) -> Result<(), PacketeerError> {
        info!(">>> {packet_handle:?} CONTAINS msg ids: {message_handles:?}");
        self.messages_in_packets
            .insert(message_handles, packet_handle.0)?;
        Ok(())
    }

    // updates ack_inbox with messages acked as a result of this packet being acks.
    // informs channels of acks so they can cleanup
    pub(crate) fn acked_packet(&mut self, packet_handle: PacketId, channel_list: &mut ChannelList) {
        // check message handles that were just acked - if any are fragments, we need to log that
        // in the frag map, incase it results in a parent message id being acked (ie, all frag messages are now acked)
        if let Some(msg_handles) = self.messages_in_packets.remove(packet_handle.0) {
            info!("Acked packet: {packet_handle:?} --> acked msgs: {msg_handles:?}");
            for msg_handle in &msg_handles {
                // let channel know, so it doesn't retransmit this message:
                channel_list
                    .get_mut(msg_handle.channel)
                    .unwrap()
                    .message_ack_received(msg_handle);
                if let Some(parent_id) = msg_handle.parent_id() {
                    // fragment message
                    if self.sent_frag_map.ack_fragment_message(
                        parent_id,
                        msg_handle.id(), // .frag_index
                                         // .expect("used to calc parent id, so must exist"),
                    ) {
                        self.ack_inbox
                            .entry(msg_handle.channel)
                            .or_default()
                            .push(parent_id);
                    } else {
                        info!("got fragment ack for parent {parent_id:?}, but not all yet {msg_handle:?} ");
                    }
                } else {
                    // non-fragment messages directly map to an acked message
                    self.ack_inbox
                        .entry(msg_handle.channel)
                        .or_default()
                        .push(msg_handle.id());
                }
            }
        }
    }

    pub(crate) fn add_message_to_channel(
        &mut self,
        pool: &BufPool,
        channel: &mut Channel,
        payload: &[u8],
    ) -> Result<MessageId, PacketeerError> {
        if payload.len() <= 1024 {
            let id = self.next_message_id();
            channel.enqueue_message(pool, id, payload, Fragmented::No);
            Ok(id)
        } else {
            self.add_large_message_to_channel(pool, channel, payload)
        }
    }

    fn add_large_message_to_channel(
        &mut self,
        pool: &BufPool,
        channel: &mut Channel,
        payload: &[u8],
    ) -> Result<MessageId, PacketeerError> {
        assert!(payload.len() > 1024);
        // all fragments use the same message id.
        let full_payload_size = payload.len();
        // split into multiple messages.
        // each fragment has a unique message id, but due to sequential allocation you can always
        // calculate the id of the first fragment - ie the one that's returned to the user for acking -
        // by taking the fragment id and subtracting the index.
        //
        // ie the message.id of the fragment with index 0 is the parent ackable message id.
        let remainder = if full_payload_size % 1024 > 0 { 1 } else { 0 };
        let num_fragments = ((full_payload_size / 1024) + remainder) as u16;
        let mut frag_ids = Vec::new();
        // id of first frag message is the parent id for the group
        let mut id = self.next_message_id();
        let parent_id = id;
        for index in 0..num_fragments {
            let payload_size = if index == num_fragments - 1 {
                full_payload_size as u16 - (num_fragments - 1) * 1024
            } else {
                1024_u16
            };
            if index > 0 {
                id = self.next_message_id();
            }
            frag_ids.push(id);
            info!("Adding frag msg {id:?} frag:{index}/{num_fragments}");
            let fragment = Fragment {
                index,
                num_fragments,
                parent_id,
            };
            let start = index as usize * 1024;
            let end = start + payload_size as usize;
            let frag_payload = &payload[start..end];
            channel.enqueue_message(pool, id, frag_payload, Fragmented::Yes(fragment));
        }
        self.sent_frag_map
            .insert_fragmented_message(parent_id, frag_ids)?;
        Ok(parent_id)
    }

    fn next_message_id(&mut self) -> MessageId {
        let ret = self.next_message_id;
        self.next_message_id = self.next_message_id.wrapping_add(1);
        MessageId(ret)
    }
}

#[derive(Default, Clone, PartialEq)]
pub(crate) enum FragAckStatus {
    #[default]
    Unknown,
    Complete,
    // lists remaining msg ids we need:
    Partial(Vec<MessageId>),
}

/// SendFragMap tracks the unacked message ids assigned to fragments of a larger message id we sent.
/// They're removed as they are acked & once depleted, the original parent message id is acked.
pub struct SentFragMap {
    m: SequenceBuffer<FragAckStatus>,
}

impl SentFragMap {
    pub(crate) fn with_capacity(size: usize) -> Self {
        Self {
            m: SequenceBuffer::with_capacity(size),
        }
    }
    pub(crate) fn insert_fragmented_message(
        &mut self,
        id: MessageId,
        fragment_ids: Vec<MessageId>,
    ) -> Result<(), PacketeerError> {
        match self.m.insert(FragAckStatus::Partial(fragment_ids), id.0) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }
    /// returns true if parent message is whole/acked.
    pub fn ack_fragment_message(&mut self, parent_id: MessageId, fragment_id: MessageId) -> bool {
        let Some(entry) = self.m.get_mut(parent_id.0) else {
            return false;
        };
        let ret = match entry {
            FragAckStatus::Complete => {
                trace!("Message {parent_id:?} already completed arrived.");
                false
            }
            FragAckStatus::Unknown => {
                warn!("Message {parent_id:?} unknown to frag map");
                false
            }
            FragAckStatus::Partial(ref mut remaining) => {
                remaining.retain(|id| *id != fragment_id);
                info!("Remaining fragment indexs for parent {parent_id:?}, fragment_id={fragment_id:?} = {remaining:?}");
                remaining.is_empty()
            }
        };
        if ret {
            self.m.insert(FragAckStatus::Complete, parent_id.0).unwrap();
            trace!("Message fully acked, all fragments accounted for {parent_id:?}");
        }
        ret
    }
}
