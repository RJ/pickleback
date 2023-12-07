use std::ops::{Deref, DerefMut};

/// Configure a pool of buffers for internal use
#[derive(Copy, Clone, Debug)]
pub struct PoolConfig {
    /// How many buffers to preallocate and fill the pool with on startup
    starting_size: usize,
    /// Maximum size of the pool before returned buffers are dropped
    max_size: usize,
    /// Size to pass to Vec::with_capacity when creating a new underlying buffer for this pool
    buffer_capacity: usize,
}

/// A newtype of Vec<u8> representing a reusable buffer that should be returned to the pool
#[derive(Clone, Default)]
pub(crate) struct PooledBuffer {
    v: Vec<u8>,
    id: u32,
}

impl std::fmt::Debug for PooledBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PooledBuffer{{id:{}, capacity:{}}}",
            self.id,
            self.v.capacity()
        )
    }
}

impl PooledBuffer {
    pub(crate) fn new(id: u32, v: Vec<u8>) -> Self {
        Self { v, id }
    }

    #[allow(unused)]
    pub(crate) fn id(&self) -> u32 {
        self.id
    }
}

impl Deref for PooledBuffer {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.v
    }
}
impl DerefMut for PooledBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.v
    }
}

pub(crate) struct Pool {
    buffers: Vec<PooledBuffer>,
    config: PoolConfig,
    overflow_allocations: usize,
    discarded_checkins: usize,
    allocation_seq: u32,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("overflow_allocations", &self.overflow_allocations)
            .field("discarded_checkins", &self.discarded_checkins)
            .field("allocation_seq", &self.allocation_seq)
            .field("buffers_len", &self.buffers.len())
            .field("config", &self.config)
            .finish()
    }
}

impl Pool {
    fn new(config: PoolConfig) -> Self {
        let mut buffers = Vec::with_capacity(config.starting_size);
        for id in 0..config.starting_size {
            buffers.push(PooledBuffer::new(
                id as u32,
                Vec::<u8>::with_capacity(config.buffer_capacity),
            ));
        }
        Self {
            buffers,
            config,
            overflow_allocations: 0,
            discarded_checkins: 0,
            allocation_seq: config.starting_size as u32,
        }
    }
    fn checkout(&mut self) -> PooledBuffer {
        if let Some(mut existing) = self.buffers.pop() {
            existing.clear();
            existing
        } else {
            self.overflow_allocations += 1;
            self.allocation_seq += 1;
            PooledBuffer::new(
                self.allocation_seq,
                Vec::<u8>::with_capacity(self.config.buffer_capacity),
            )
        }
    }
    fn checkin(&mut self, buffer: PooledBuffer) {
        if self.buffers.len() >= self.config.max_size {
            self.discarded_checkins += 1;
        } else {
            self.buffers.push(buffer);
        }
    }
}

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
    pools: Vec<(PoolConfig, Pool)>,
}

impl std::fmt::Debug for BufPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (_conf, p) in &self.pools {
            writeln!(f, "{p:?}")?;
        }
        Ok(())
    }
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
            .map(|config| (*config, Pool::new(*config)))
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

    /// Gets a buffer from a pool of vecs with an initial capacity of at least `min_size`
    pub(crate) fn get_buffer(&mut self, min_size: usize) -> PooledBuffer {
        log::warn!("Getting buffer, cap {min_size}");
        for (config, pool) in self.pools.iter_mut() {
            if config.buffer_capacity >= min_size || config.buffer_capacity == 0 {
                return pool.checkout();
            }
        }
        // anything larger always comes from the final pool, and perhaps you end up reallocating
        // space in the vecs in that pool, for really big messages.
        self.pools.last_mut().unwrap().1.checkout()
    }

    /// Checks the buffer back in to the pool so it can be reused
    pub(crate) fn return_buffer(&mut self, buffer: PooledBuffer) {
        let min_size = buffer.capacity();
        log::warn!("Returning buffer, cap {min_size}");
        for (config, pool) in self.pools.iter_mut() {
            if config.buffer_capacity >= min_size || config.buffer_capacity == 0 {
                pool.checkin(buffer);
                return;
            }
        }
        // anything larger always comes from the final pool, and perhaps you end up reallocating
        // space in the vecs in that pool, for really big messages.
        self.pools.last_mut().unwrap().1.checkin(buffer);
    }
}
