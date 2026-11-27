//! 시각과 난수. 시험에서 바꿔 끼울 수 있게 둘만 포트로 뺀다.

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
    /// 버퍼를 예측할 수 없는 바이트로 채운다. 못 하면 false.
    fn fill(&self, buf: &mut [u8]) -> bool;
}

/// 운영체제 CSPRNG(`bot_core::random`).
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
