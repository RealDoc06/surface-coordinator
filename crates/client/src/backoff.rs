//! Reconnect delays: exponential backoff with jitter.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct BackoffConfig {
    /// Upper bound of the first delay.
    pub initial: Duration,
    /// Cap on any delay.
    pub max: Duration,
    /// A connection that stayed up this long resets the backoff. Shorter
    /// ones do not, so a coordinator that accepts and then drops the client
    /// is not hammered.
    pub stable_after: Duration,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        BackoffConfig {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            stable_after: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
pub struct Backoff {
    config: BackoffConfig,
    attempt: u32,
}

impl Backoff {
    pub fn new(config: BackoffConfig) -> Backoff {
        Backoff { config, attempt: 0 }
    }

    /// The next delay. `random` is a uniform sample from `[0, 1)`.
    ///
    /// The ceiling doubles with every attempt up to the cap; the delay is
    /// drawn from the upper half of the ceiling, so clients that lost the
    /// coordinator together do not come back together.
    pub fn next_delay_with(&mut self, random: f64) -> Duration {
        let doubled = self.config.initial.saturating_mul(2u32.saturating_pow(self.attempt));
        let ceiling = doubled.min(self.config.max);
        self.attempt = self.attempt.saturating_add(1);
        let half = ceiling / 2;
        half + half.mul_f64(random.clamp(0.0, 1.0))
    }

    pub fn next_delay(&mut self) -> Duration {
        self.next_delay_with(rand::random::<f64>())
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backoff() -> Backoff {
        Backoff::new(BackoffConfig::default())
    }

    #[test]
    fn ceiling_doubles_until_the_cap() {
        let mut backoff = backoff();
        // With the largest jitter the delay is the ceiling itself.
        let ceilings: Vec<u64> = (0..9).map(|_| backoff.next_delay_with(1.0).as_secs()).collect();
        assert_eq!(ceilings, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn jitter_stays_within_the_upper_half_of_the_ceiling() {
        let mut low = backoff();
        let mut high = backoff();
        for ceiling_secs in [1u64, 2, 4, 8, 16, 32, 60, 60] {
            let ceiling = Duration::from_secs(ceiling_secs);
            assert_eq!(low.next_delay_with(0.0), ceiling / 2);
            assert_eq!(high.next_delay_with(1.0), ceiling);
        }
        let mut mid = backoff();
        assert_eq!(mid.next_delay_with(0.5), Duration::from_millis(750));
    }

    #[test]
    fn random_delays_never_exceed_sixty_seconds() {
        let mut backoff = backoff();
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..200 {
            let delay = backoff.next_delay();
            assert!(delay <= Duration::from_secs(60), "{delay:?}");
            distinct.insert(delay);
        }
        // Capped delays are still spread out by the jitter.
        assert!(distinct.len() > 100);
    }

    #[test]
    fn reset_starts_over() {
        let mut backoff = backoff();
        for _ in 0..5 {
            backoff.next_delay_with(1.0);
        }
        backoff.reset();
        assert_eq!(backoff.next_delay_with(1.0), Duration::from_secs(1));
    }

    #[test]
    fn many_attempts_do_not_overflow() {
        let mut backoff = backoff();
        for _ in 0..10_000 {
            assert!(backoff.next_delay_with(1.0) <= Duration::from_secs(60));
        }
    }
}
