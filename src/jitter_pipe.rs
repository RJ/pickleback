// a queue that can reorder, drop, and duplicate packets, like sending udp packets over the internet.
// not testing packet modification; assuming there are checksums so mangled packets manifest as drops.
use std::{cmp::Ordering, collections::BinaryHeap};

/// Packets are assigned a float sort key upon insert, based off a counter that increments +1.0
/// for each packet sent. Jitter modifies the sort key by +/- . So Jitter under 0.5 will never
/// cause packets to reorder, since packets with keys 1,2 with jitter of 0.4 will at worse become:
/// 1.4, 1.6 but worst case jitter of 0.6 could be: 1.6, 1.4 which would cause a reorder.
#[derive(Clone)]
pub struct JitterPipeConfig {
    pub enabled: bool,
    pub drop_chance: f32,
    pub duplicate_chance: f32,
    // any jitter less than 0.5 will not cause reordering.
    // need to expose this setting in a scaled, more useful way somehow.
    pub max_jitter: f32,
}

impl Default for JitterPipeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            drop_chance: 0.01,
            duplicate_chance: 0.001,
            max_jitter: 0.75,
        }
    }
}

impl JitterPipeConfig {
    pub fn bad() -> Self {
        Self {
            enabled: true,
            drop_chance: 0.05,
            duplicate_chance: 0.005,
            max_jitter: 2.0,
        }
    }

    pub fn terrible() -> Self {
        Self {
            enabled: true,
            drop_chance: 0.075,
            duplicate_chance: 0.01,
            max_jitter: 5.0,
        }
    }

    #[allow(unused)]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Default::default()
        }
    }
    fn should_drop(&self) -> bool {
        self.enabled && rand::random::<f32>() < self.drop_chance
    }
    fn should_duplicate(&self) -> bool {
        self.enabled && rand::random::<f32>() < self.duplicate_chance
    }
    fn jitter(&self) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        rand::random::<f32>() * 2.0 * self.max_jitter - self.max_jitter
    }
}

pub struct JitterPipe<T: Eq + PartialEq + Clone> {
    heap: BinaryHeap<SortWrapper<T>>,
    key_seq: f32,
    config: JitterPipeConfig,
}

#[allow(unused)]
impl<T: Eq + PartialEq + Clone> JitterPipe<T> {
    pub fn new(config: JitterPipeConfig) -> Self {
        Self {
            config,
            heap: BinaryHeap::new(),
            key_seq: 0.0,
        }
    }
    pub fn config_mut(&mut self) -> &mut JitterPipeConfig {
        &mut self.config
    }
    pub fn next_key(&mut self) -> f32 {
        self.key_seq += 1.0;
        self.key_seq + self.config.jitter()
    }
    pub fn insert(&mut self, item: T) {
        if self.config.should_drop() {
            return;
        }
        if self.config.should_duplicate() {
            let key = self.next_key();
            self.heap.push(SortWrapper {
                key,
                item: item.clone(),
            });
        }
        let key = self.next_key();
        self.heap.push(SortWrapper { key, item });
    }
    pub fn take_next(&mut self) -> Option<T> {
        if let Some(SortWrapper { item, .. }) = self.heap.pop() {
            return Some(item);
        }
        None
    }
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

struct SortWrapper<T: PartialEq + Eq + Clone> {
    key: f32,
    item: T,
}

impl<T: PartialEq + Eq + Clone> PartialEq for SortWrapper<T> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.item == other.item
    }
}

impl<T: PartialEq + Eq + Clone> Eq for SortWrapper<T> {}

impl<T: PartialEq + Eq + Clone> Ord for SortWrapper<T> {
    fn cmp(&self, other: &SortWrapper<T>) -> Ordering {
        if self.key == other.key {
            return Ordering::Equal;
        }
        if self.key < other.key {
            return Ordering::Greater;
        }
        Ordering::Less
    }
}

impl<T: PartialEq + Eq + Clone> PartialOrd for SortWrapper<T> {
    fn partial_cmp(&self, other: &SortWrapper<T>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_pipe_disabled() {
        crate::test_utils::init_logger();
        let mut jp = JitterPipe::<u32>::new(JitterPipeConfig::disabled());
        for i in 0..1000 {
            jp.insert(i);
        }
        for i in 0..1000 {
            assert_eq!(i, jp.take_next().unwrap());
        }
        assert!(jp.take_next().is_none());
    }

    // testing this properly requires a lot of faff.
    // as long as it causes havok, it's kind of doing its job..
}
