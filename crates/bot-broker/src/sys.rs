//! Clock and CSPRNG entropy ports for deterministic testing.

use std::time::SystemTime;

pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

pub trait Entropy: Send + Sync {
    /// Fills `buf` with unpredictable bytes; returns `false` on failure.
    fn fill(&self, buf: &mut [u8]) -> bool;
}

pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn fill(&self, buf: &mut [u8]) -> bool {
        bot_core::random::fill(buf);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_entropy_fills_and_differs() {
        let (mut a, mut b) = ([0u8; 16], [0u8; 16]);
        assert!(OsEntropy.fill(&mut a) && OsEntropy.fill(&mut b));
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 16]);
    }
}
