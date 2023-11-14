use crate::*;

/// Indexed by message id in the seen_buf of a channel
// #[derive(Default, Clone)]
// pub(crate) enum ReceivedMessageStatus {
//     #[default]
//     Empty,
//     FullyReceived,
//     TooOld,
//     InProgress(IncompleteMessage),
// }

// // all vec entries will be size of largest enum, so change from array to vec for frag storage?
// pub(crate) struct SeenBuffer {
//     buf: SequenceBuffer<ReceivedMessageStatus>,
// }

// impl SeenBuffer {
//     fn with_capacity(size: usize) -> Self {
//         Self {
//             buf: SequenceBuffer::with_capacity(size),
//         }
//     }
//     fn get_mut(&mut self, id: MessageId) -> &mut ReceivedMessageStatus {
//         if !self.buf.check_sequence(id) {
//             return ReceivedMessageStatus::TooOld;
//         }
//         match self.buf.get_mut(id) {
//             Some(entry) => entry,
//             None => ReceivedMessageStatus::Empty,
//         }
//     }
// }

/// Represents a mesage being assembled from fragments.
/// once all fragments provided, a normal Message is produced.
///
/// max fragments set to 1024 for now.
/// could be a dynamically size vec based on num-fragments tho..
#[derive(Clone, Debug)]
pub(crate) struct IncompleteMessage {
    channel: u8,
    id: MessageId,
    num_fragments: u16,
    num_received_fragments: u16,
    fragments: [Option<Bytes>; 1024], // TODO make vec
}

// TODO i think it's resending every fragment if the parent isn't acked yet?

impl IncompleteMessage {
    pub(crate) fn new(channel: u8, id: MessageId, num_fragments: u16) -> Self {
        assert!(
            num_fragments <= 1024,
            "num fragments can't exceed 1024 for arbitrary reasons atm"
        );
        Self {
            channel,
            id,
            num_fragments,
            num_received_fragments: 0,
            fragments: std::array::from_fn(|_| None),
        }
    }
    pub(crate) fn add_fragment(
        &mut self,
        frag_index: u16,
        payload: Bytes,
    ) -> Option<ReceivedMessage> {
        assert!(frag_index < 1024);
        if self.fragments[frag_index as usize].is_some() {
            warn!("Already seen this fragment, discarding");
            return None;
        }
        self.fragments[frag_index as usize] = Some(payload);
        self.num_received_fragments += 1;
        if self.num_received_fragments == self.num_fragments {
            let size = (self.num_fragments as usize - 1) * 1024
                + self.fragments[self.num_fragments as usize - 1]
                    .as_ref()
                    .unwrap()
                    .len();
            let mut reassembled_payload = BytesMut::with_capacity(size);
            for i in 0..self.num_fragments {
                reassembled_payload.extend_from_slice(self.fragments[i as usize].as_ref().unwrap());
            }
            return Some(ReceivedMessage {
                channel: self.channel,
                payload: reassembled_payload.freeze(),
                id: self.id,
            });
        }
        None
    }
}

// us seqbuffer of enum{ in_prog(incomp msg), finished, empty} ?
#[derive(Default)]
pub(crate) struct MessageReassembler {
    in_progress: HashMap<MessageId, IncompleteMessage>,
}

impl MessageReassembler {
    pub(crate) fn add_fragment(&mut self, message: &Message) -> Option<ReceivedMessage> {
        let Some(fragment) = message.fragment() else {
            // an unfragmented message, can return immediately.
            return Some(ReceivedMessage {
                channel: message.channel(),
                payload: message.payload().clone(),
                id: message.id(),
            });
        };
        let parent_id = message
            .fragment()
            .map(|f| f.parent_id)
            .expect("Can't find fragment parent id");

        let ret = match self.in_progress.entry(parent_id) {
            Entry::Occupied(entry) => entry
                .into_mut()
                .add_fragment(fragment.index, message.payload().clone()),
            Entry::Vacant(v) => {
                let incomp_msg = v.insert(IncompleteMessage::new(
                    message.channel(),
                    message.id(),
                    fragment.num_fragments,
                ));
                incomp_msg.add_fragment(fragment.index, message.payload().clone())
            }
        };

        if ret.is_some() {
            // resulted in a fully reassembled message, cleanup:
            info!("🧩 Reassembly complete for {parent_id}, having just received {fragment:?} ");
            self.in_progress.remove(&parent_id);
        }
        ret
    }
}
