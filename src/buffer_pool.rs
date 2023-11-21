use lifeguard::*;

pub type BufHandle = RcRecycled<Vec<u8>>;

pub struct BufPool {
    pool: Pool<Vec<u8>>,
}

// TODO different size pools
impl BufPool {
    pub fn new() -> Self {
        let pool = pool()
            .with(StartingSize(10))
            .with(MaxSize(999999))
            .with(Supplier(|| Vec::<u8>::with_capacity(1300)))
            .build();

        Self { pool }
    }

    pub(crate) fn get_buffer(&self, _min_size: usize) -> BufHandle {
        self.pool.new_rc()
    }
}
