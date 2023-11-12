use crate::*;
// use log::*;

#[derive(Default, Debug)]
pub(crate) struct MessageHandle {
    pub(crate) id: MessageId,
    pub(crate) parent: Option<MessageId>,
    pub(crate) channel: u8,
}
impl MessageHandle {
    pub fn id(&self) -> MessageId {
        self.id
    }
    pub(crate) fn parent_id(&self) -> Option<MessageId> {
        self.parent
    }
}

#[derive(Default)]
pub(crate) struct MessageDispatcher {
    next_message_id: MessageId,
    sent_frag_map: SentFragMap,
    next_frag_group_id: u8,
    messages_in_packets: HashMap<SentHandle, Vec<MessageHandle>>,
    message_reassembler: MessageReassembler,
    /// received fully assembled messages ready for the consumer:
    message_inbox: HashMap<u8, Vec<ReceivedMessage>>,
    ack_inbox: HashMap<u8, Vec<MessageId>>,
}

impl MessageDispatcher {
    // pass in msgs parsed from received network packets.
    pub(crate) fn process_received_message(&mut self, msg: Message) {
        info!("Dispatcher::process_received_message: {msg:?}");
        // in case it's a fragment, do reassembly.
        // this just yields a ReceivedMessage instantly for non-fragments:
        if let Some(received_msg) = self.message_reassembler.add_fragment(&msg) {
            info!(
                "Adding message to inbox for channel {}",
                received_msg.channel
            );
            self.message_inbox
                .entry(received_msg.channel)
                .or_default()
                .push(received_msg);
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
        packet_handle: SentHandle,
        message_handles: Vec<MessageHandle>,
    ) {
        self.messages_in_packets
            .insert(packet_handle, message_handles);
    }

    // updates ack_inbox with messages acked as a result of this packet being acks.
    // informs channels of acks so they can cleanup
    pub(crate) fn acked_packet(
        &mut self,
        packet_handle: &SentHandle,
        channel_list: &mut ChannelList,
    ) {
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
                        channel_list
                            .get_mut(msg_handle.channel)
                            .unwrap()
                            .message_ack_received(parent_id);
                        self.ack_inbox
                            .entry(msg_handle.channel)
                            .or_default()
                            .push(parent_id);
                    }
                } else {
                    channel_list
                        .get_mut(msg_handle.channel)
                        .unwrap()
                        .message_ack_received(msg_handle.id());
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
        channel: &mut Box<dyn Channel>,
        payload: Bytes,
    ) -> MessageId {
        if payload.len() <= 1024 {
            self.add_small_message_to_channel(channel, payload)
        } else {
            self.add_large_message_to_channel(channel, payload)
        }
    }

    fn add_small_message_to_channel(
        &mut self,
        channel: &mut Box<dyn Channel>,
        payload: Bytes,
    ) -> MessageId {
        assert!(payload.len() <= 1024);
        let id = self.next_message_id();
        let msg = Message::new_unfragmented(id, channel.id(), payload);
        channel.enqueue_message(msg);
        id
    }

    fn add_large_message_to_channel(
        &mut self,
        channel: &mut Box<dyn Channel>,
        mut payload: Bytes,
    ) -> MessageId {
        assert!(payload.len() > 1024);
        // this is the message id we return to the user, upon which they await an ack.
        let parent_id = self.next_message_id();
        let payload_len = payload.len();
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
                channel.id(),
                frag_payload,
                fragment_group_id,
                fragment_id as u16,
                num_fragments as u16,
                parent_id,
            );
            fragment_ids.push(fragment_message_id);
            channel.enqueue_message(msg);
        }
        self.sent_frag_map
            .insert_fragmented_message(parent_id, fragment_ids);

        parent_id
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
