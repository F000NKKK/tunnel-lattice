//! Heap allocations per received packet on both `PacketStream` paths,
//! measured with a counting global allocator.
//!
//! `harness = false`: this binary's `main` runs the cases in order on one
//! thread, so no libtest machinery allocates concurrently. `cargo test`
//! name filters do not apply to it.
//!
//! Each stream case is differential. It runs the same path for `K1` and
//! `K2` packets, each time counting from before the stream (and its pool)
//! is built until the mock device has been dropped (quiescence), and
//! asserts on `allocs(K2) - allocs(K1)`. Fixed per-stream costs (the pool's
//! slab and slot flags, thread spawn, channel and box setup, the pool's
//! waiter lists growing on their first use) cancel out, leaving the
//! per-packet cost of `K2 - K1` packets, which must be zero.
//!
//! The send case needs no difference: the provided `send_batch` (sync, and
//! async under a warmed-up `block_on`) has no per-call setup, so sending
//! `K2` packets in batches must not allocate at all.

use std::alloc::{GlobalAlloc, Layout, System};
use std::future::{Future, ready};
use std::hint::black_box;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use futures::executor::block_on;
use futures::{Stream, StreamExt};
use tunnel_lattice_async::{
    Error, PacketBuf, PacketPool, PacketStream, Result, from_async_device,
    from_async_device_with_pool, from_device, from_device_with_pool,
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
        // Decrement-if-positive as an explicit CAS loop: `fetch_update` is
        // deprecated on newer toolchains, and its `try_update` replacement is
        // newer than the workspace MSRV.
        let mut n = self.remaining.load(Ordering::Acquire);
        let took = loop {
            let Some(next) = n.checked_sub(1) else {
                break false;
            };
            match self
                .remaining
                .compare_exchange_weak(n, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break true,
                Err(current) => n = current,
            }
        };
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

/// Which pool a run builds, inside its measured window.
#[derive(Clone, Copy, Debug)]
enum Slots {
    /// The default constructors' `PacketPool::with_buf_len(BUF_LEN)`.
    Default,
    /// `PacketPool::new(n, BUF_LEN)` through the `*_with_pool` constructors.
    Exactly(usize),
}

/// Runs one stream over `count` packets to quiescence. Returns the number
/// of allocations from before construction until the device was dropped.
fn run(path: Path, count: usize, slots: Slots) -> u64 {
    let (device, dropped) = FiniteDevice::new(count);
    let before = allocs();
    let mut stream = match (path, slots) {
        (Path::Native, Slots::Default) => from_async_device(device, BUF_LEN).unwrap(),
        (Path::Bridge, Slots::Default) => from_device(device, BUF_LEN).unwrap(),
        (Path::Native, Slots::Exactly(n)) => {
            from_async_device_with_pool(device, PacketPool::new(n, BUF_LEN).unwrap())
        }
        (Path::Bridge, Slots::Exactly(n)) => {
            from_device_with_pool(device, PacketPool::new(n, BUF_LEN).unwrap())
        }
    };
    drain(&mut stream, count);
    drop(stream);
    wait_dropped(&dropped);
    allocs() - before
}

fn delta(path: Path) -> u64 {
    let k1 = run(path, K1, Slots::Default);
    let k2 = run(path, K2, Slots::Default);
    k2 - k1
}

/// Counts wakes.
#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// What [`run_exhausted`] observed.
struct Exhausted {
    allocs: u64,
    pendings: usize,
    wakes: usize,
}

/// Runs a native stream over a 1-slot pool to quiescence, polling it by
/// hand and holding each packet until the next poll returns `Pending`, so
/// every receive waits for the slot: the acquire registers a waiter, and
/// dropping the held packet wakes it through the release slow path.
fn run_exhausted(count: usize) -> Exhausted {
    let (device, dropped) = FiniteDevice::new(count);
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(Arc::clone(&counter));
    let mut cx = Context::from_waker(&waker);

    let before = allocs();
    let pool = PacketPool::new(1, BUF_LEN).unwrap();
    let mut stream = from_async_device_with_pool(device, pool);
    let mut held: Option<PacketBuf> = None;
    let (mut packets, mut pendings, mut disconnects) = (0usize, 0usize, 0usize);
    loop {
        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Ready(Some(Ok(packet))) => {
                assert!(held.is_none(), "a packet arrived while the slot was held");
                assert_eq!(packet.len(), PAYLOAD);
                held = Some(black_box(packet));
                packets += 1;
            }
            Poll::Pending => {
                let released = held.take().expect("pending only while the slot is held");
                drop(released);
                pendings += 1;
            }
            Poll::Ready(Some(Err(Error::Disconnected))) => disconnects += 1,
            Poll::Ready(Some(Err(err))) => panic!("unexpected stream error: {err:?}"),
            Poll::Ready(None) => break,
        }
    }
    drop(stream);
    wait_dropped(&dropped);
    let allocs = allocs() - before;
    assert_eq!((packets, disconnects), (count, 1));
    Exhausted {
        allocs,
        pendings,
        wakes: counter.0.load(Ordering::SeqCst),
    }
}

/// Every lazy, process-wide or thread-local initialisation that a stream
/// case would otherwise pay inside its first measured window.
fn warm_up() {
    block_on(async {});
    // First spawn reads `RUST_MIN_STACK` once and caches it.
    thread::spawn(|| ()).join().unwrap();
    run(Path::Native, K0, Slots::Default);
    run(Path::Bridge, K0, Slots::Default);
    run_exhausted(K0);
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

    // The pool's slab is its only zeroed allocation (the slot flags and the
    // shared state are plain allocations; eager mutex/condvar init adds
    // plain ones on pthread platforms only).
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

/// Every rejected size, and both default constructors given a rejected
/// `buf_len`, fail without allocating; the constructors drop the device.
fn pool_new_rejects_without_alloc() {
    let before = allocs();
    for (slots, buf_len) in [
        (0, BUF_LEN),
        (16, 0),
        (usize::MAX, 1),
        (1, usize::MAX),
        (65_536, 65_535),
    ] {
        assert!(matches!(
            black_box(PacketPool::new(slots, buf_len)),
            Err(Error::InvalidState)
        ));
    }
    for buf_len in [0, usize::MAX] {
        assert!(matches!(
            black_box(PacketPool::with_buf_len(buf_len)),
            Err(Error::InvalidState)
        ));
    }
    assert_eq!(allocs() - before, 0, "a rejected size must not allocate");

    for path in [Path::Native, Path::Bridge] {
        let (device, dropped) = FiniteDevice::new(0);
        let before = allocs();
        let result = match path {
            Path::Native => black_box(from_async_device(device, 0)),
            Path::Bridge => black_box(from_device(device, 0)),
        };
        assert!(matches!(result, Err(Error::InvalidState)));
        drop(result);
        assert_eq!(
            allocs() - before,
            0,
            "{path:?}: a rejected buf_len must not allocate"
        );
        assert!(
            dropped.load(Ordering::Acquire),
            "{path:?}: the device is dropped"
        );
    }
    println!("pool_new_rejects_without_alloc: ok");
}

fn native_stream_zero_alloc_steady_state() {
    let d = delta(Path::Native);
    println!("native_stream_zero_alloc_steady_state: delta={d}");
    assert_eq!(d, 0, "the native stream must not allocate per packet");
}

/// The native stream stays allocation-free when every receive waits for a
/// slot. The pending and wake counts prove each receive really took that
/// path (a waiter registered, then woken by the release).
fn native_stream_exhausted_zero_alloc() {
    let k1 = run_exhausted(K1);
    let k2 = run_exhausted(K2);
    for (run, count) in [(&k1, K1), (&k2, K2)] {
        assert_eq!(run.pendings, count, "every receive must wait for the slot");
        assert_eq!(run.wakes, count, "every release must wake the waiter");
    }
    let d = k2.allocs - k1.allocs;
    println!(
        "native_stream_exhausted_zero_alloc: delta={d} pendings={} wakes={}",
        k2.pendings, k2.wakes
    );
    assert_eq!(
        d, 0,
        "an exhausted native stream must not allocate per packet"
    );
}

fn bridge_stream_zero_alloc_steady_state() {
    let d = delta(Path::Bridge);
    println!("bridge_stream_zero_alloc_steady_state: delta={d}");
    assert_eq!(d, 0, "the thread bridge must not allocate per packet");
}

/// Packets per `send_batch` call in [`send_batch_zero_alloc`].
const SEND_BATCH: usize = 64;

/// The provided `PacketIo::send_batch` and `AsyncPacketIo::send_batch`
/// send `K2` packets, in batches, without a single allocation.
fn send_batch_zero_alloc() {
    let (device, _dropped) = FiniteDevice::new(0);
    let payload = [0xA5u8; PAYLOAD];
    let refs: [&[u8]; SEND_BATCH] = [&payload; SEND_BATCH];
    for asynchronous in [false, true] {
        let before = allocs();
        let mut sent = 0;
        while sent < K2 {
            let packets = &refs[..SEND_BATCH.min(K2 - sent)];
            let n = if asynchronous {
                block_on(AsyncPacketIo::send_batch(&*device, black_box(packets)))
            } else {
                PacketIo::send_batch(&*device, black_box(packets))
            }
            .expect("the mock accepts every send");
            assert_eq!(n, packets.len(), "the default sends the whole batch");
            sent += n;
        }
        let d = allocs() - before;
        println!("send_batch_zero_alloc: async={asynchronous} packets={sent} allocs={d}");
        assert_eq!(
            d, 0,
            "send_batch (async {asynchronous}) must not allocate per packet"
        );
    }
}

/// Stride of a 1500-byte slot: `align_up(1500 + 1, 64)`.
const STRIDE: usize = 1536;

/// The owned bytes a stream over `slots` slots is documented to hold: the
/// slab with its alignment slack and the slot flags (one byte each), plus
/// the bridge's channel slots.
fn expected_bytes(path: Path, slots: usize) -> i64 {
    let pool = slots * STRIDE + 63 + slots;
    let channel = match path {
        Path::Native => 0,
        Path::Bridge => slots * (size_of::<usize>() + size_of::<Result<PacketBuf>>()),
    };
    (pool + channel) as i64
}

/// The peak bytes a stream owns stay within its documented bound, and grow
/// with the slot count exactly as that bound does.
fn stream_owned_peak_bytes() {
    const TOLERANCE: i64 = 4 * 1024;
    for path in [Path::Native, Path::Bridge] {
        let mut peaks = [0i64; 2];
        for (peak, slots) in peaks.iter_mut().zip([16, 128]) {
            let live_start = rebase_peak();
            run(path, K1, Slots::Exactly(slots));
            *peak = PEAK.load(Ordering::SeqCst) - live_start;
            let expected = expected_bytes(path, slots);
            println!(
                "stream_owned_peak_bytes: {path:?} slots={slots} peak={peak} expected={expected}"
            );
            assert!(
                *peak <= expected + TOLERANCE,
                "{path:?}, {slots} slots: peak {peak} exceeds {expected} + {TOLERANCE}"
            );
        }
        let growth = peaks[1] - peaks[0];
        let expected_growth = expected_bytes(path, 128) - expected_bytes(path, 16);
        assert!(
            (growth - expected_growth).abs() <= TOLERANCE,
            "{path:?}: peak grew by {growth}, expected {expected_growth} +/- {TOLERANCE}"
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
    pool_new_rejects_without_alloc();
    native_stream_zero_alloc_steady_state();
    native_stream_exhausted_zero_alloc();
    bridge_stream_zero_alloc_steady_state();
    send_batch_zero_alloc();
    stream_owned_peak_bytes();
}
