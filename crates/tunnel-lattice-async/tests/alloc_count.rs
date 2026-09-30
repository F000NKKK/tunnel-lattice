//! Heap allocations per received packet on both `PacketStream` paths,
//! measured with a counting global allocator.
//!
//! `harness = false`: this binary's `main` runs the cases in order on one
//! thread, so no libtest machinery allocates concurrently. `cargo test`
//! name filters do not apply to it.
//!
//! Each stream case is differential. It runs the same path for `K1` and
//! `K2` packets, each time counting from before the stream is built until
//! the mock device has been dropped (quiescence), and asserts on
//! `allocs(K2) - allocs(K1)`. Fixed per-stream costs (thread spawn, channel
//! and box setup) cancel out, leaving the per-packet cost of
//! `K2 - K1` packets.

use std::alloc::{GlobalAlloc, Layout, System};
use std::future::{Future, ready};
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::executor::block_on;
use tunnel_lattice_async::{
    Error, PacketPool, PacketStream, Result, from_async_device, from_device,
};
use tunnel_lattice_platform::{AsyncPacketIo, PacketIo};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ZEROED: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

fn grow(bytes: usize) {
    let live = LIVE.fetch_add(bytes as i64, Ordering::Relaxed) + bytes as i64;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn shrink(bytes: usize) {
    LIVE.fetch_sub(bytes as i64, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged; the counters are side bookkeeping only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged; the caller upholds `alloc`'s contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ZEROED.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged; the caller upholds `alloc_zeroed`'s contract.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        shrink(layout.size());
        // SAFETY: forwarded unchanged; the caller upholds `dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged; the caller upholds `realloc`'s contract.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        // On failure the old block stays allocated, so live bytes are unchanged.
        if !new.is_null() {
            shrink(layout.size());
            grow(new_size);
        }
        new
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs() -> u64 {
    ALLOCS.load(Ordering::SeqCst)
}

fn zeroed() -> u64 {
    ZEROED.load(Ordering::SeqCst)
}

/// Starts a peak-bytes window: returns the live byte count the peak is
/// measured from.
fn rebase_peak() -> i64 {
    let live = LIVE.load(Ordering::SeqCst);
    PEAK.store(live, Ordering::SeqCst);
    live
}

const PAYLOAD: usize = 64;
const BUF_LEN: usize = 1500;
const K0: usize = 100;
const K1: usize = 1_000;
const K2: usize = 11_000;
const QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(30);

/// `count` packets of [`PAYLOAD`] bytes, then `Disconnected`. Sets
/// `dropped` when the last `Arc` to it goes away, which is how a case
/// knows the bridge worker thread has let go of everything it held.
struct FiniteDevice {
    remaining: AtomicUsize,
    dropped: Arc<AtomicBool>,
}

impl FiniteDevice {
    fn new(count: usize) -> (Arc<Self>, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let device = Arc::new(Self {
            remaining: AtomicUsize::new(count),
            dropped: Arc::clone(&dropped),
        });
        (device, dropped)
    }

    fn recv_now(&self, buf: &mut [u8]) -> Result<usize> {
        let took = self
            .remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok();
        if !took {
            return Err(Error::Disconnected);
        }
        buf[..PAYLOAD].fill(0xA5);
        Ok(PAYLOAD)
    }
}

impl Drop for FiniteDevice {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

impl PacketIo for FiniteDevice {
    fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.recv_now(buf)
    }

    fn send(&self, buf: &[u8]) -> Result<usize> {
        Ok(buf.len())
    }
}

impl AsyncPacketIo for FiniteDevice {
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
        ready(self.recv_now(buf))
    }

    fn send(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send {
        ready(Ok(buf.len()))
    }
}

#[derive(Clone, Copy, Debug)]
enum Path {
    Native,
    Bridge,
}

/// Drains a stream to its end, asserting it yields exactly `count` packets
/// followed by one `Disconnected`.
fn drain(stream: &mut PacketStream, count: usize) {
    let (packets, disconnects) = block_on(async {
        let (mut packets, mut disconnects) = (0usize, 0usize);
        while let Some(item) = stream.next().await {
            match item {
                Ok(packet) => {
                    assert_eq!(packet.len(), PAYLOAD);
                    black_box(&packet);
                    packets += 1;
                }
                Err(Error::Disconnected) => disconnects += 1,
                Err(err) => panic!("unexpected stream error: {err:?}"),
            }
        }
        (packets, disconnects)
    });
    assert_eq!((packets, disconnects), (count, 1));
}

fn wait_dropped(dropped: &AtomicBool) {
    let start = Instant::now();
    while !dropped.load(Ordering::Acquire) {
        assert!(
            start.elapsed() < QUIESCENCE_TIMEOUT,
            "mock device was not dropped within {QUIESCENCE_TIMEOUT:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// Runs one stream over `count` packets to quiescence. Returns the number
/// of allocations from before construction until the device was dropped.
fn run(path: Path, count: usize) -> u64 {
    let (device, dropped) = FiniteDevice::new(count);
    let before = allocs();
    let mut stream = match path {
        Path::Native => from_async_device(device, BUF_LEN),
        Path::Bridge => from_device(device, BUF_LEN),
    };
    drain(&mut stream, count);
    drop(stream);
    wait_dropped(&dropped);
    allocs() - before
}

fn delta(path: Path) -> u64 {
    let k1 = run(path, K1);
    let k2 = run(path, K2);
    k2 - k1
}

/// Every lazy, process-wide or thread-local initialisation that a stream
/// case would otherwise pay inside its first measured window.
fn warm_up() {
    block_on(async {});
    // First spawn reads `RUST_MIN_STACK` once and caches it.
    thread::spawn(|| ()).join().unwrap();
    run(Path::Native, K0);
    run(Path::Bridge, K0);
    // Initialise stdout's buffer before any case measures.
    println!("alloc_count: warmed up");
}

/// The counters and executor behave as the stream cases assume.
fn executor_control() {
    let before = allocs();
    block_on(async {});
    assert_eq!(
        allocs() - before,
        0,
        "a warmed-up block_on must not allocate"
    );

    let before = zeroed();
    drop(black_box(vec![0u8; BUF_LEN]));
    assert_eq!(zeroed() - before, 1, "vec![0; n] must be one alloc_zeroed");

    // The pool's slab is its only zeroed allocation (the free list and the
    // `Arc` are plain allocations; eager mutex/condvar init adds plain ones
    // on pthread platforms only).
    let before = zeroed();
    drop(black_box(
        PacketPool::new(128, BUF_LEN).expect("128 slots of 1500 bytes are valid"),
    ));
    assert_eq!(
        zeroed() - before,
        1,
        "PacketPool::new must make exactly one alloc_zeroed (the slab)"
    );
    println!("executor_control: ok");
}

/// Documents the 0.4 per-packet cost: one zeroed `Vec` per packet on the
/// native path; a `Vec` copy plus a channel node per packet on the bridge.
fn baseline_vec_documented() {
    let packets = (K2 - K1) as u64;
    for (path, per_packet) in [(Path::Native, 1), (Path::Bridge, 2)] {
        let live_start = rebase_peak();
        let d = delta(path);
        let peak = PEAK.load(Ordering::SeqCst) - live_start;
        println!("baseline_vec_documented: {path:?} delta={d} peak_bytes={peak}");
        assert_eq!(
            d,
            per_packet * packets,
            "{path:?}: expected {per_packet} allocation(s) per packet"
        );
    }
}

fn main() {
    // A counting global allocator plus a real thread per case is slow and
    // pointless under Miri; soundness of the pool is checked by unit tests.
    if cfg!(miri) {
        return;
    }
    warm_up();
    executor_control();
    baseline_vec_documented();
}
