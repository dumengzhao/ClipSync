//! 冲突解决 - Lamport 逻辑时钟
//!
//! 阶段一实现

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LamportClock {
    counter: u64,
}

impl LamportClock {
    pub fn new() -> Self {
        Self { counter: 0 }
    }

    pub fn tick(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    pub fn observe(&mut self, other: u64) -> u64 {
        self.counter = self.counter.max(other) + 1;
        self.counter
    }

    pub fn current(&self) -> u64 {
        self.counter
    }
}

impl Default for LamportClock {
    fn default() -> Self {
        Self::new()
    }
}
