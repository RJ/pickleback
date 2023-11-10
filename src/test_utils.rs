#[allow(unused)]
pub(crate) fn init_logger() {
    let _ = env_logger::builder()
        .write_style(env_logger::WriteStyle::Always)
        .is_test(true)
        .try_init();
}
