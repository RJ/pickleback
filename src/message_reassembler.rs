use crate::*;

/// Represents a mesage being assembled from fragments.
/// once all fragments provided, a normal Message is produced.
///
/// max fragments set to 1024 for now.
/// could be a dynamically size vec based on num-fragments tho..
pub(crate) struct IncompleteMessage {
    channel: u8,
    num_fragments: u16,
    num_received_fragments: u16,
    fragments: [Option<Bytes>; 1024],
}

impl IncompleteMessage {
    pub(crate) fn new(channel: u8, num_fragments: u16) -> Self {
        assert!(
            num_fragments <= 1024,
            "num fragments can't exceed 1024 for arbitrary reasons atm"
        );
        Self {
            channel,
            num_fragments,
            num_received_fragments: 0,
            fragments: std::array::from_fn(|_| None),
        }
    }
    pub(crate) fn add_fragment(
        &mut self,
        fragment_id: u16,
        payload: Bytes,
    ) -> Option<ReceivedMessage> {
        assert!(fragment_id < 1024);
        if self.fragments[fragment_id as usize].is_some() {
            warn!("Already seen this fragment, discarding");
            return None;
        }
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
                reassembled_payload.extend_from_slice(self.fragments[i as usize].as_ref().unwrap());
            }
            return Some(ReceivedMessage {
                channel: self.channel,
                payload: reassembled_payload.freeze(),
            });
        }
        None
    }
}

#[derive(Default)]
pub(crate) struct MessageReassembler {
    in_progress: HashMap<FragGroupId, IncompleteMessage>,
}

impl MessageReassembler {
    pub(crate) fn add_fragment(&mut self, message: &Message) -> Option<ReceivedMessage> {
        let Some(fragment) = message.fragment() else {
            // an unfragmented message, can return immediately.
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
