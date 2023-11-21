use lifeguard::*;

#[derive(Copy, Clone)]
pub struct PoolConfig {
    starting_size: usize,
    max_size: usize,
    buffer_capacity: usize,
}

pub type BufHandle = RcRecycled<Vec<u8>>;

/// `BufPool` manages multiple pools of Vec<u8> buffers, with varying capacities.
/// When you request a pooled buffer, you ask for something with a certain maximum capacity,
/// and one is returned from the appropriate pool for that capacity.
///
/// Even though they are Vecs and can reallocate, as long as you specify the max size correctly,
/// you'll never cause a vec to reallocate.
pub struct BufPool {
    pools: Vec<(PoolConfig, Pool<Vec<u8>>)>,
}

impl Default for BufPool {
    fn default() -> Self {
        let configs = vec![
            PoolConfig {
                starting_size: 1000,
                max_size: 10000,
                buffer_capacity: 50,
            },
            PoolConfig {
                starting_size: 1000,
                max_size: 10000,
                buffer_capacity: 260,
            },
            PoolConfig {
                starting_size: 100,
                max_size: 1000,
                buffer_capacity: 1200,
            },
            PoolConfig {
                starting_size: 10,
                max_size: 100,
                buffer_capacity: 10000,
            },
            PoolConfig {
                starting_size: 10,
                max_size: 100,
                buffer_capacity: 100000,
            },
            PoolConfig {
                starting_size: 1,
                max_size: 10,
                buffer_capacity: crate::MAX_MESSAGE_LEN,
            },
        ];
        Self::new(configs)
    }
}
// TODO different size pools
impl BufPool {
    pub(crate) fn new(mut configs: Vec<PoolConfig>) -> Self {
        configs.sort_by_key(|c| c.buffer_capacity);
        let pools = configs
            .iter()
            .map(|config| (*config, Self::new_pool(config)))
            .collect::<Vec<_>>();
        Self { pools }
    }

    /// The empty pool always allocates a Vec with default capacity
    #[allow(dead_code)]
    pub(crate) fn empty() -> Self {
        let configs = vec![PoolConfig {
            starting_size: 0,
            max_size: 0,
            buffer_capacity: 0,
        }];
        Self::new(configs)
    }

    fn new_pool(config: &PoolConfig) -> Pool<Vec<u8>> {
        let PoolConfig {
            starting_size,
            max_size,
            buffer_capacity,
        } = config;
        let capacity = *buffer_capacity;
        pool()
            .with(StartingSize(*starting_size))
            .with(MaxSize(*max_size))
            .with(Supplier(move || {
                if capacity > 0 {
                    Vec::<u8>::with_capacity(capacity)
                } else {
                    Vec::new()
                }
            }))
            .build()
    }

    pub fn get_buffer(&self, min_size: usize) -> BufHandle {
        for (config, pool) in self.pools.iter() {
            if config.buffer_capacity >= min_size || config.buffer_capacity == 0 {
                return pool.new_rc();
            }
        }
        log::error!("No buffer pool exists that can allocate for size {min_size}, using largest pool anyway.");
        self.pools.last().unwrap().1.new_rc()
    }
}
