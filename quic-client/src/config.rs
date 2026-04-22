use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize, Debug, Clone)]
pub struct QuicSettings {
    pub max_timeout_secs: u64,
    pub keep_alive_secs: u64,
    pub num_endpoints: usize,
    pub max_send_attempts: usize,
    pub retry_delay_ms: u64,
    pub send_timeout_secs: u64,
}

impl Default for QuicSettings {
    fn default() -> Self {
        Self {
            max_timeout_secs: 30,
            keep_alive_secs: 1,
            num_endpoints: 1,
            max_send_attempts: 3,
            retry_delay_ms: 100,
            send_timeout_secs: 5,
        }
    }
}

impl QuicSettings {
    pub fn max_timeout(&self) -> Duration {
        Duration::from_secs(self.max_timeout_secs)
    }

    pub fn keep_alive(&self) -> Duration {
        Duration::from_secs(self.keep_alive_secs)
    }

    pub fn retry_delay(&self) -> Duration {
        Duration::from_millis(self.retry_delay_ms)
    }

    pub fn send_timeout(&self) -> Duration {
        Duration::from_secs(self.send_timeout_secs)
    }

    pub fn set_num_endpoint(&mut self, num_endpoints: usize) {
        self.num_endpoints = num_endpoints;
    }
}
