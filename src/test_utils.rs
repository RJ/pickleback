use bytes::{BufMut, Bytes, BytesMut};

#[allow(unused)]
pub(crate) fn init_logger() {
    let _ = env_logger::builder()
        .write_style(env_logger::WriteStyle::Always)
        // .is_test(true)
        .try_init();
}

#[allow(unused)]
pub(crate) fn random_payload(size: u32) -> Bytes {
    let mut b = BytesMut::with_capacity(size as usize);
    for _ in 0..size {
        b.put_u8(rand::random::<u8>());
    }
    b.freeze()
}
