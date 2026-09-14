//! Random bytes for `--build-id=uuid`, without a dependency.
//!
//! A UUID build-id is deliberately not reproducible: it changes on every
//! link, which is its whole point, so this is the one place in the writer
//! where output depends on something other than the inputs.
//!
//! Sources, in order:
//!
//! 1. On Unix, 16 bytes read from `/dev/urandom`, which never blocks after
//!    boot and is available even in minimal containers and chroots that have
//!    `/dev`.
//! 2. Otherwise (Windows, or `/dev/urandom` missing), SHA-1 over several
//!    weak and strong entropy sources: the keys of fresh
//!    `std::collections::hash_map::RandomState`s, which the standard library
//!    seeds from the operating system's CSPRNG (`ProcessPrng` on Windows,
//!    `getrandom`/`arc4random` on Unix), plus the wall clock, the process
//!    id, a thread id, a per-process counter and a stack address.
//!
//! Neither needs to be cryptographically strong; a build-id only needs to
//! be unique in practice.

use super::hash::Sha1;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Returns a random version 4 UUID (RFC 9562), in byte order.
#[must_use]
pub fn random_uuid() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    if !fill_from_os(&mut bytes) {
        bytes = mixed_entropy();
    }
    // Version 4, variant 10xx.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

#[cfg(unix)]
fn fill_from_os(buf: &mut [u8; 16]) -> bool {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(buf))
        .is_ok()
}

#[cfg(not(unix))]
fn fill_from_os(_buf: &mut [u8; 16]) -> bool {
    false
}

fn mixed_entropy() -> [u8; 16] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut sha = Sha1::new();
    for _ in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        sha.update(&hasher.finish().to_le_bytes());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    sha.update(&now.to_le_bytes());
    sha.update(&std::process::id().to_le_bytes());
    sha.update(format!("{:?}", std::thread::current().id()).as_bytes());
    let stack_marker = 0u8;
    sha.update(&(std::ptr::addr_of!(stack_marker) as usize).to_le_bytes());
    let digest = sha.finalize();
    let mut out = [0u8; 16];
    for (dst, src) in out.iter_mut().zip(digest) {
        *dst = src;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_version_4_and_differ() {
        let a = random_uuid();
        let b = random_uuid();
        assert_ne!(a, b);
        for uuid in [a, b] {
            assert_eq!(uuid[6] >> 4, 4);
            assert_eq!(uuid[8] >> 6, 0b10);
        }
    }

    #[test]
    fn fallback_differs_between_calls() {
        assert_ne!(mixed_entropy(), mixed_entropy());
    }
}
