use lifeguard::*;

/// Configure a pool of buffers for internal use
#[derive(Copy, Clone)]
pub struct PoolConfig {
    /// How many buffers to preallocate and fill the pool with on startup
    starting_size: usize,
    /// Maximum size of the pool before returned buffers are dropped
    max_size: usize,
    /// Size to pass to Vec::with_capacity when creating a new underlying buffer for this pool
    buffer_capacity: usize,
}

/// A reference counted smart pointer to a pooled buffer, managed by Pickleback's Buffer Pool.
pub type BufHandle = RcRecycled<Vec<u8>>;

/// `BufPool` manages multiple pools of Vec<u8> buffers, with varying capacities.
/// When you request a pooled buffer, you ask for something with a certain minimum capacity,
/// and one is returned from the appropriate pool for that capacity.
///
/// Even though they are Vecs and can reallocate, as long as you specify the max size correctly,
/// you'll never cause a vec to reallocate.
///
/// The exception to this is if you ask for a min capacity larger than all the pools, you get a
/// vec from the final pool and you will cause reallocations. ie, the final pool is reserved for
/// very large occasional messages.
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
            // the last one is where anything that doesn't fit is pulled from, and
            // it's possible vecs from here reallocate if you need larger messages.
            // potentially up to ~ config.max_message_size
            PoolConfig {
                starting_size: 0,
                max_size: 10,
                buffer_capacity: 100001,
            },
        ];
        Self::new(configs)
    }
}

impl BufPool {
    pub(crate) fn new(mut configs: Vec<PoolConfig>) -> Self {
        configs.sort_by_key(|c| c.buffer_capacity);
        let pools = configs
            .iter()
            .map(|config| (*config, Self::new_pool(config)))
            .collect::<Vec<_>>();
        Self { pools }
    }

    pub(crate) fn full_packets_only() -> Self {
        let pools = vec![PoolConfig {
            starting_size: 1,
            max_size: 10,
            buffer_capacity: 1500,
        }];
        Self::new(pools)
    }

    /// The empty pool always allocates a Vec with default capacity
    /// This is mostly for tests.
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

    /// Returns a buffer from a pool of vecs with an initial capacity of at least `min_size`
    pub fn get_buffer(&self, min_size: usize) -> BufHandle {
        for (config, pool) in self.pools.iter() {
            if config.buffer_capacity >= min_size || config.buffer_capacity == 0 {
                return pool.new_rc();
            }
        }
        // anything larger always comes from the final pool, and perhaps you end up reallocating
        // space in the vecs in that pool, for really big messages.
        self.pools.last().unwrap().1.new_rc()
    }
}
