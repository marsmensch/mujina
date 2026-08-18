//! Apollo III board virtual transport (startup-only).
//!
//! Like the CPU miner transport, Apollo events are synthesized at startup
//! from environment configuration (`MUJINA_APOLLO_SERIAL` presence) rather
//! than discovered from hardware: the daemon emits exactly one connect
//! event, then an enumeration-complete event, and the channel closes.

/// Transport events for the Apollo III board.
#[derive(Debug)]
pub enum TransportEvent {
    /// The Apollo III board was enabled via the environment.
    ApolloDeviceConnected(ApolloDeviceInfo),

    /// The Apollo III board was disconnected.
    ApolloDeviceDisconnected { device_id: String },
}

/// Information about the Apollo III virtual device.
#[derive(Debug, Clone)]
pub struct ApolloDeviceInfo {
    /// Unique identifier for this virtual device (also the board id used
    /// by the backplane).
    pub device_id: String,
}
