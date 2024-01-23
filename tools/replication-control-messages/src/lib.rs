use std::{net::SocketAddr, time::Duration};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub throughput: f32,
    pub latency: Duration,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Protocol {
    Unreplicated,
    Pbft,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub protocol: Protocol,
    pub replica_addrs: Vec<SocketAddr>,
    pub num_replica: usize,
    pub num_faulty: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaConfig {
    pub protocol: Protocol,
    pub replica_id: u8,
    pub replica_addrs: Vec<SocketAddr>,
    pub num_replica: usize,
    pub num_faulty: usize,
}
