//! Bounded in-process replay cache — the fallback used when Redis is absent.
//!
//! Redis is the authoritative, cross-worker replay gate. When it is not
//! configured, each worker falls back to this: single-worker protection, which
//! is what the documentation promises operators who run without Redis.
//!
//! Eviction is FIFO, not a flush. Clearing the whole set at capacity would let
//! an attacker push `cap` fresh credentials through and then replay an older
//! one, which defeats the purpose of keeping the set at all.

use std::collections::{HashSet, VecDeque};

/// Default number of entries held per worker.
pub const DEFAULT_REPLAY_CACHE_CAP: usize = 10_000;

/// A bounded set of already-spent credential keys.
///
/// Keys are opaque strings — callers pass a *hash* of the credential (see
/// `preimage_redis_key`), never the raw secret, so the cache holds nothing
/// worth stealing.
#[derive(Debug)]
pub struct ReplayCache {
    seen: HashSet<String>,
    order: VecDeque<String>,
    cap: usize,
}

impl ReplayCache {
    /// Create a cache holding at most `cap` entries. A `cap` of 0 is raised to
    /// 1: a zero-capacity cache would admit every replay.
    pub fn with_capacity(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            seen: HashSet::with_capacity(cap),
            order: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Claim `key`. Returns `true` on first sighting (caller may admit) and
    /// `false` if it was already claimed (caller must reject as a replay).
    ///
    /// At capacity the oldest entry is evicted, so a credential can only be
    /// replayed after `cap` distinct others have been seen — the bound the
    /// operator accepts by running without Redis.
    pub fn claim(&mut self, key: &str) -> bool {
        if self.seen.contains(key) {
            return false;
        }
        if self.order.len() >= self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.order.push_back(key.to_string());
        self.seen.insert(key.to_string());
        true
    }

    /// Whether `key` has been claimed. Read-only; does not claim.
    pub fn contains(&self, key: &str) -> bool {
        self.seen.contains(key)
    }

    /// Undo a claim, so a credential whose guarded work then failed can be
    /// retried instead of being stuck "used". Mirrors the Redis path's
    /// `release_cashu_token`; a key that was never claimed is left alone.
    pub fn release(&mut self, key: &str) {
        if self.seen.remove(key) {
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                self.order.remove(pos);
            }
        }
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

impl Default for ReplayCache {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_REPLAY_CACHE_CAP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_claim_succeeds_and_second_is_a_replay() {
        let mut c = ReplayCache::with_capacity(8);
        assert!(c.claim("a"), "first sighting must be admitted");
        assert!(!c.claim("a"), "second sighting must be rejected");
    }

    #[test]
    fn distinct_keys_do_not_collide() {
        let mut c = ReplayCache::with_capacity(8);
        assert!(c.claim("a"));
        assert!(c.claim("b"));
        assert!(!c.claim("a"));
        assert!(!c.claim("b"));
    }

    #[test]
    fn contains_does_not_claim() {
        let mut c = ReplayCache::with_capacity(4);
        assert!(!c.contains("a"));
        assert!(c.claim("a"));
        assert!(c.contains("a"));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn stays_within_capacity() {
        let mut c = ReplayCache::with_capacity(4);
        for i in 0..100 {
            c.claim(&format!("k{i}"));
        }
        assert_eq!(c.len(), 4, "cache must not grow past its capacity");
    }

    /// The property that matters: eviction drops the OLDEST entry, so recent
    /// credentials stay protected. A flush-at-capacity implementation fails
    /// this — after the flush every earlier key is replayable again.
    #[test]
    fn eviction_is_fifo_not_a_flush() {
        let mut c = ReplayCache::with_capacity(3);
        c.claim("oldest");
        c.claim("middle");
        c.claim("newest");

        c.claim("overflow"); // evicts "oldest" only

        assert!(!c.contains("oldest"), "oldest should have been evicted");
        assert!(c.contains("middle"), "middle must survive");
        assert!(c.contains("newest"), "newest must survive");
        assert!(c.contains("overflow"));
        assert!(!c.claim("newest"), "a surviving key is still a replay");
    }

    /// A flush-at-capacity cache lets an attacker push `cap` throwaway
    /// credentials and then reuse a spent one. FIFO makes that cost linear in
    /// the number of *later* credentials rather than resetting the whole set.
    #[test]
    fn flooding_evicts_gradually_rather_than_resetting() {
        let mut c = ReplayCache::with_capacity(10);
        c.claim("spent");

        for i in 0..9 {
            c.claim(&format!("filler{i}"));
        }
        assert!(
            !c.claim("spent"),
            "still protected while it remains resident"
        );

        c.claim("filler9");
        assert!(!c.contains("spent"));
    }

    #[test]
    fn zero_capacity_is_raised_to_one() {
        let mut c = ReplayCache::with_capacity(0);
        assert!(c.claim("a"));
        assert!(!c.claim("a"), "a zero-cap cache must not admit replays");
    }

    #[test]
    fn default_uses_the_documented_capacity() {
        assert_eq!(ReplayCache::default().len(), 0);
        assert_eq!(DEFAULT_REPLAY_CACHE_CAP, 10_000);
    }

    #[test]
    fn release_lets_a_failed_claim_be_retried() {
        let mut c = ReplayCache::with_capacity(8);
        assert!(c.claim("a"));
        c.release("a");
        assert!(!c.contains("a"));
        assert!(c.claim("a"), "a released key must be claimable again");
    }

    /// Release must drop the key from the FIFO order too, or the freed slot
    /// stays occupied and eviction pushes out a still-live entry early.
    #[test]
    fn release_frees_the_capacity_slot() {
        let mut c = ReplayCache::with_capacity(2);
        assert!(c.claim("a"));
        assert!(c.claim("b"));
        c.release("a");
        assert_eq!(c.len(), 1);
        assert!(c.claim("c"));
        assert!(
            c.contains("b"),
            "b must survive — a's slot was the free one"
        );
    }

    #[test]
    fn releasing_an_unclaimed_key_is_a_no_op() {
        let mut c = ReplayCache::with_capacity(4);
        assert!(c.claim("a"));
        c.release("never-claimed");
        assert_eq!(c.len(), 1);
        assert!(c.contains("a"));
    }
}
