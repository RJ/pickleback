use crate::*;

pub const MAX_FRAGMENTS: usize = 1024;

/// Represents a mesage being assembled from fragments.
/// once all fragments provided, a normal Message is produced.
///
/// max fragments set to 1024 for now.
/// could be a dynamically size vec based on num-fragments tho..
#[derive(Clone, Debug)]
pub(crate) struct IncompleteMessage {
    num_fragments: u16,
    num_received_fragments: u16,
    fragments: Vec<Option<Message>>,
}

impl IncompleteMessage {
    pub(crate) fn new(num_fragments: u16) -> Self {
        assert!(
            num_fragments as usize <= MAX_FRAGMENTS,
            "num fragments can't exceed 1024 for arbitrary reasons atm"
        );
        assert!(
            num_fragments > 1,
            "Fragmented messages must have at least 2 fragments!"
        );
        Self {
            num_fragments,
            num_received_fragments: 0,
            fragments: vec![None; num_fragments as usize],
        }
    }
    pub(crate) fn add_fragment(&mut self, frag_index: u16, message: Message) -> bool {
        assert!(frag_index as usize <= MAX_FRAGMENTS);
        assert!(frag_index < self.num_fragments);
        if self.fragments[frag_index as usize].is_some() {
            warn!("Already seen this fragment, discarding");
            return false;
        }
        self.fragments[frag_index as usize] = Some(message);
        self.num_received_fragments += 1;
        // got final fragment?
        self.num_received_fragments == self.num_fragments
    }

    fn take_fragments(&mut self) -> Vec<Option<Message>> {
        std::mem::take(&mut self.fragments)
    }
}

// us seqbuffer of enum{ in_prog(incomp msg), finished, empty} ?
#[derive(Default)]
pub(crate) struct MessageReassembler {
    in_progress: HashMap<MessageId, IncompleteMessage>,
}

impl MessageReassembler {
    pub(crate) fn add_fragment(&mut self, message: Message) -> Option<ReceivedMessage> {
        let Some(fragment) = message.fragment() else {
            panic!("don't pass unfragmented messages to the message reassembler!");
        };

        let parent_id = message
            .fragment()
            .map(|f| f.parent_id)
            .expect("Can't find fragment parent id");

        let ready = if let Some(incomp_msg) = self.in_progress.get_mut(&parent_id) {
            let frag_index = message.fragment().unwrap().index;
            incomp_msg.add_fragment(frag_index, message)
        } else {
            let mut incomp_msg = IncompleteMessage::new(fragment.num_fragments);
            incomp_msg.add_fragment(fragment.index, message);
            self.in_progress.insert(parent_id, incomp_msg);
            false
        };

        if ready {
            let mut incomp_msg = self.in_progress.remove(&parent_id).unwrap();
            let ret = ReceivedMessage::new_fragmented(incomp_msg.take_fragments());
            Some(ret)
        } else {
            None
        }
    }
}
