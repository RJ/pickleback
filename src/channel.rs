use std::time::Duration;

// resend time could be dynamic based on recent rtt calculations per client?

#[derive(Debug, Clone)]
pub struct ChannelConfig {
    id: u8,
    reliable: bool,
    ordered: bool,
    resend_time: Option<Duration>,
}

pub enum DefaultChannels {
    Unreliable,
    Reliable,
}

impl From<DefaultChannels> for u8 {
    fn from(value: DefaultChannels) -> Self {
        match value {
            DefaultChannels::Unreliable => 0,
            DefaultChannels::Reliable => 1,
        }
    }
}

impl DefaultChannels {
    pub fn channel_config() -> Vec<ChannelConfig> {
        vec![
            ChannelConfig {
                id: 0,
                reliable: false,
                ordered: false,
                resend_time: None,
            },
            ChannelConfig {
                id: 1,
                reliable: true,
                ordered: false,
                resend_time: Some(Duration::from_millis(100)),
            },
        ]
    }
}
