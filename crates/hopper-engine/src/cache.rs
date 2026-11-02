//! What the engine remembers about moments it has looked at: memory in aligned chunks, register
//! sets, and unwound stacks, keyed by [`Moment`]. A moment names one program state, so an entry
//! never goes stale; the cache only forgets entries to stay within its capacity (least recently
//! used first).
//!
//! On rr the cache saves seeks. On a live process it is the only way to answer for a moment the
//! process has left: whatever was read while the process was there stays answerable.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard};

use hopper_core::target::Memory;
use hopper_core::{HopperError, Moment, RegisterSet, Result, Tid};
use hopper_symbols::StackFrame;

/// Memory is cached in chunks of this many bytes, aligned to their size. A chunk never crosses a
/// page, so it is either wholly readable or not at all.
pub const CHUNK: u64 = 512;

/// A map that forgets its least recently used entries beyond a capacity.
#[derive(Debug)]
struct Lru<K, V> {
    map: HashMap<K, (V, u64)>,
    clock: u64,
    capacity: usize,
}

impl<K: Hash + Eq + Clone, V: Clone> Lru<K, V> {
    fn new(capacity: usize) -> Self {
        Lru {
            map: HashMap::new(),
            clock: 0,
            capacity: capacity.max(1),
        }
    }

    fn get(&mut self, k: &K) -> Option<V> {
        self.clock += 1;
        let now = self.clock;
        self.map.get_mut(k).map(|(v, used)| {
            *used = now;
            v.clone()
        })
    }

    fn put(&mut self, k: K, v: V) {
        self.clock += 1;
        self.map.insert(k, (v, self.clock));
        if self.map.len() > self.capacity {
            // Forget the oldest quarter at once, so eviction stays cheap on average.
            let mut ages: Vec<u64> = self.map.values().map(|(_, u)| *u).collect();
            ages.sort_unstable();
            let cut = ages[ages.len() / 4];
            self.map.retain(|_, (_, u)| *u > cut);
        }
    }
}

type Chunks = Lru<(Moment, u64), Arc<[u8]>>;
type Stacks = Lru<(Moment, Tid), Arc<Vec<StackFrame>>>;

/// The cache shared by every replica of a session.
#[derive(Debug)]
pub struct MomentCache {
    chunks: Mutex<Chunks>,
    registers: Mutex<Lru<(Moment, Tid), RegisterSet>>,
    stacks: Mutex<Stacks>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl MomentCache {
    /// A cache holding up to `chunks` memory chunks (of [`CHUNK`] bytes).
    pub fn new(chunks: usize) -> Self {
        MomentCache {
            chunks: Mutex::new(Lru::new(chunks)),
            registers: Mutex::new(Lru::new(4096)),
            stacks: Mutex::new(Lru::new(1024)),
        }
    }

    pub fn chunk(&self, at: Moment, base: u64) -> Option<Arc<[u8]>> {
        lock(&self.chunks).get(&(at, base))
    }

    pub fn put_chunk(&self, at: Moment, base: u64, bytes: Arc<[u8]>) {
        lock(&self.chunks).put((at, base), bytes);
    }

    pub fn registers(&self, at: Moment, tid: Tid) -> Option<RegisterSet> {
        lock(&self.registers).get(&(at, tid))
    }

    pub fn put_registers(&self, at: Moment, tid: Tid, regs: RegisterSet) {
        lock(&self.registers).put((at, tid), regs);
    }

    pub fn stack(&self, at: Moment, tid: Tid) -> Option<Arc<Vec<StackFrame>>> {
        lock(&self.stacks).get(&(at, tid))
    }

    pub fn put_stack(&self, at: Moment, tid: Tid, stack: Arc<Vec<StackFrame>>) {
        lock(&self.stacks).put((at, tid), stack);
    }
}

/// Where a [`MomentMemory`] gets bytes the cache does not have.
pub trait Fetch {
    /// Read `buf` at `addr` from the program state the memory describes.
    fn fetch(&mut self, addr: u64, buf: &mut [u8]) -> Result<()>;
}

/// No source but the cache: a live process that has moved on.
#[derive(Debug)]
pub struct CacheOnly {
    pub at: Moment,
}

impl Fetch for CacheOnly {
    fn fetch(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        Err(HopperError::Unsupported(format!(
            "the live process has left moment {}; {} bytes at {addr:#x} were not read while it was \
             there (a live session cannot go back)",
            self.at,
            buf.len()
        )))
    }
}

/// Memory of one moment: the cache first, then `fetch`, filling the cache in whole chunks.
pub struct MomentMemory<'a, F: Fetch> {
    pub cache: &'a MomentCache,
    pub at: Moment,
    pub source: F,
}

impl<F: Fetch> std::fmt::Debug for MomentMemory<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MomentMemory")
            .field("at", &self.at)
            .finish()
    }
}

impl<F: Fetch> Memory for MomentMemory<'_, F> {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        let len = buf.len() as u64;
        if len == 0 {
            return Ok(());
        }
        let end = addr
            .checked_add(len)
            .ok_or(HopperError::UnreadableMemory { addr, len })?;
        let mut base = addr & !(CHUNK - 1);
        while base < end {
            let chunk = match self.cache.chunk(self.at, base) {
                Some(c) => c,
                None => {
                    let mut b = vec![0u8; CHUNK as usize];
                    match self.source.fetch(base, &mut b) {
                        Ok(()) => {
                            let c: Arc<[u8]> = Arc::from(b);
                            self.cache.put_chunk(self.at, base, c.clone());
                            c
                        }
                        // Part of the chunk is unreadable (or the source cannot read it):
                        // read exactly what was asked, uncached.
                        Err(_) => {
                            let lo = addr.max(base);
                            let hi = end.min(base + CHUNK);
                            let off = (lo - addr) as usize;
                            let n = (hi - lo) as usize;
                            self.source.fetch(lo, &mut buf[off..off + n])?;
                            base += CHUNK;
                            continue;
                        }
                    }
                }
            };
            let lo = addr.max(base);
            let hi = end.min(base + CHUNK);
            let off = (lo - addr) as usize;
            let coff = (lo - base) as usize;
            let n = (hi - lo) as usize;
            buf[off..off + n].copy_from_slice(&chunk[coff..coff + n]);
            base += CHUNK;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::RecordingId;

    struct Counting {
        reads: u32,
    }

    impl Fetch for Counting {
        fn fetch(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
            self.reads += 1;
            if addr >= 0x2000 {
                return Err(HopperError::UnreadableMemory {
                    addr,
                    len: buf.len() as u64,
                });
            }
            for (i, b) in buf.iter_mut().enumerate() {
                *b = (addr as usize + i) as u8;
            }
            Ok(())
        }
    }

    fn m(sub: u32) -> Moment {
        Moment {
            recording: RecordingId([1; 16]),
            frame_time: 1,
            tid: Tid(1),
            ticks: 0,
            sub,
        }
    }

    #[test]
    fn reads_through_and_then_from_the_cache() {
        let cache = MomentCache::new(16);
        let mut mem = MomentMemory {
            cache: &cache,
            at: m(0),
            source: Counting { reads: 0 },
        };
        let mut b = [0u8; 8];
        mem.read(0x1fc, &mut b).unwrap(); // crosses two chunks
        assert_eq!(b, [0xfc, 0xfd, 0xfe, 0xff, 0, 1, 2, 3]);
        assert_eq!(mem.source.reads, 2);
        mem.read(0x200, &mut b).unwrap();
        assert_eq!(mem.source.reads, 2, "served from the cache");
        // Another moment does not share entries.
        let mut other = MomentMemory {
            cache: &cache,
            at: m(1),
            source: CacheOnly { at: m(1) },
        };
        assert!(matches!(
            other.read(0x200, &mut b),
            Err(HopperError::Unsupported(_))
        ));
        let mut same = MomentMemory {
            cache: &cache,
            at: m(0),
            source: CacheOnly { at: m(0) },
        };
        same.read(0x200, &mut b).unwrap();
        assert_eq!(b[0], 0);
    }

    #[test]
    fn unreadable_chunks_fall_back_to_exact_reads() {
        let cache = MomentCache::new(16);
        let mut mem = MomentMemory {
            cache: &cache,
            at: m(0),
            source: Counting { reads: 0 },
        };
        let mut b = [0u8; 4];
        assert!(mem.read(0x2000, &mut b).is_err());
    }

    #[test]
    fn lru_forgets_the_oldest() {
        let mut l = Lru::new(4);
        for i in 0..4 {
            l.put(i, i);
        }
        l.get(&0);
        l.put(4, 4);
        assert!(l.get(&0).is_some(), "recently used survives");
        assert!(l.get(&1).is_none(), "oldest is forgotten");
    }
}
