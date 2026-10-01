//! Packet receive-path throughput over in-memory mock devices.
//!
//! Every measured iteration receives [`support::N`] packets from a finite
//! mock (`N` packets, then `Disconnected`) and drains the stream to its
//! end. Building the device and stream, and dropping them afterwards, are
//! outside the timed routine (`iter_batched` with `PerIteration`).
//!
//! Timing bias: both thread-bridge mocks (the frozen baseline and the
//! pooled `from_device`) are gated, and setup waits until the worker thread
//! is blocked on the gate, so the worker neither starts up nor receives
//! anything inside the timed routine until it opens the gate. The timed
//! window therefore covers all `N` packets on both the native and the
//! bridge paths, plus one worker wake-up from the gate on the bridge paths.
//! Without the gate the baseline bridge (an unbounded channel) could
//! receive every packet during setup and time only the consumer, and the
//! pooled bridge could receive up to one pool's worth of packets.
//!
//! Groups:
//!
//! - `sync_recv_caller_buf`: the lower bound, a synchronous `recv` loop
//!   into one reused buffer.
//! - `baseline_vec_native` / `baseline_vec_bridge`: a frozen copy of the
//!   earlier `Vec`-per-packet streams (see `support/baseline.rs`).
//! - `stream_native_pool` / `stream_bridge_pool`: today's `PacketStream`
//!   from `from_async_device` / `from_device`, dropping each packet at once.
//! - `stream_native_pool_shared`: as `stream_native_pool`, but through
//!   `from_async_device_with_pool` while the setup keeps a clone of the
//!   pool alive, so the stream claims slots in the pool's shared mode.
//! - `stream_native_pool_hold/{1,32,127}`: the native stream at 1500/1500
//!   while the consumer keeps the last `H` packets alive (the default pool
//!   has 128 slots), so released slots are not the most recently used.
//! - `stream_native_imix`: the native stream over a 7:4:1 mix of 64-, 576-
//!   and 1500-byte packets in a 1500-byte buffer (`N` = 9,996, 833 rounds).
//!
//! Run with `cargo bench -p tunnel-lattice-async`. Numbers from mocks
//! measure this crate's own per-packet overhead (allocation, copies,
//! channel, wake-ups), not device throughput.

mod support;

use std::collections::VecDeque;
use std::future::{Future, ready};
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures::StreamExt;
use futures::executor::block_on;
use tunnel_lattice_async::{
    Error, PacketPool, PacketStream, Result, from_async_device, from_async_device_with_pool,
    from_device,
};
use tunnel_lattice_platform::{AsyncPacketIo, PacketIo};

use support::{FiniteDevice, N, PAIRS, baseline};

fn id(payload: usize, buf_len: usize) -> BenchmarkId {
    BenchmarkId::from_parameter(format!("{payload}/{buf_len}"))
}

fn throughput(payload: usize) -> Throughput {
    Throughput::ElementsAndBytes {
        elements: N as u64,
        bytes: (N * payload) as u64,
    }
}

fn drain(stream: &mut baseline::VecStream) {
    block_on(async {
        while let Some(item) = stream.next().await {
            if let Ok(packet) = item {
                black_box(&packet);
            }
        }
    });
}

/// Lower bound: synchronous `PacketIo::recv` into one reused caller buffer.
fn sync_recv_caller_buf(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_recv_caller_buf");
    for (payload, buf_len) in PAIRS {
        group.throughput(throughput(payload));
        group.bench_function(id(payload, buf_len), |b| {
            b.iter_batched(
                || (FiniteDevice::new(N, payload), vec![0u8; buf_len]),
                |(device, mut buf)| {
                    while let Ok(n) = PacketIo::recv(&device, &mut buf) {
                        black_box(&buf[..n]);
                    }
                    (device, buf)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// 0.4 native stream: one zeroed `Vec` per packet.
fn baseline_vec_native(c: &mut Criterion) {
    let mut group = c.benchmark_group("baseline_vec_native");
    for (payload, buf_len) in PAIRS {
        group.throughput(throughput(payload));
        group.bench_function(id(payload, buf_len), |b| {
            b.iter_batched(
                || baseline::native(Arc::new(FiniteDevice::new(N, payload)), buf_len),
                |mut stream| {
                    drain(&mut stream);
                    stream
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// 0.4 thread bridge: a `Vec` copy plus an unbounded-channel node per packet.
fn baseline_vec_bridge(c: &mut Criterion) {
    let mut group = c.benchmark_group("baseline_vec_bridge");
    for (payload, buf_len) in PAIRS {
        group.throughput(throughput(payload));
        group.bench_function(id(payload, buf_len), |b| {
            b.iter_batched(
                || {
                    let device = Arc::new(FiniteDevice::gated(N, payload));
                    let stream = baseline::bridge(Arc::clone(&device), buf_len);
                    device.wait_arrived();
                    (device, stream)
                },
                |(device, mut stream)| {
                    device.open_gate();
                    drain(&mut stream);
                    (device, stream)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn drain_pooled(stream: &mut PacketStream) {
    block_on(async {
        while let Some(item) = stream.next().await {
            if let Ok(packet) = item {
                black_box(&packet);
            }
        }
    });
}

/// `PacketStream` from `from_async_device`: each packet received into a
/// pool slot and dropped at once.
fn stream_native_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_native_pool");
    for (payload, buf_len) in PAIRS {
        group.throughput(throughput(payload));
        group.bench_function(id(payload, buf_len), |b| {
            b.iter_batched(
                || {
                    from_async_device(Arc::new(FiniteDevice::new(N, payload)), buf_len)
                        .expect("valid buf_len")
                },
                |mut stream| {
                    drain_pooled(&mut stream);
                    stream
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// `PacketStream` from `from_async_device_with_pool` with a second pool
/// handle kept alive: the shared (compare-exchange) claim mode.
fn stream_native_pool_shared(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_native_pool_shared");
    for (payload, buf_len) in PAIRS {
        group.throughput(throughput(payload));
        group.bench_function(id(payload, buf_len), |b| {
            b.iter_batched(
                || {
                    let pool = PacketPool::with_buf_len(buf_len).expect("valid buf_len");
                    let stream = from_async_device_with_pool(
                        Arc::new(FiniteDevice::new(N, payload)),
                        pool.clone(),
                    );
                    (stream, pool)
                },
                |(mut stream, pool)| {
                    drain_pooled(&mut stream);
                    (stream, pool)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// `PacketStream` from `from_device`, gated like the baseline bridge.
fn stream_bridge_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_bridge_pool");
    for (payload, buf_len) in PAIRS {
        group.throughput(throughput(payload));
        group.bench_function(id(payload, buf_len), |b| {
            b.iter_batched(
                || {
                    let device = Arc::new(FiniteDevice::gated(N, payload));
                    let stream = from_device(Arc::clone(&device), buf_len).expect("valid buf_len");
                    device.wait_arrived();
                    (device, stream)
                },
                |(device, mut stream)| {
                    device.open_gate();
                    drain_pooled(&mut stream);
                    (device, stream)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// The native stream while the consumer keeps the last `hold` packets.
fn stream_native_pool_hold(c: &mut Criterion) {
    const PAYLOAD: usize = 1500;
    const BUF_LEN: usize = 1500;
    let mut group = c.benchmark_group("stream_native_pool_hold");
    group.throughput(throughput(PAYLOAD));
    for hold in [1usize, 32, 127] {
        group.bench_function(BenchmarkId::from_parameter(hold), |b| {
            b.iter_batched(
                || {
                    let stream =
                        from_async_device(Arc::new(FiniteDevice::new(N, PAYLOAD)), BUF_LEN)
                            .expect("valid buf_len");
                    (stream, VecDeque::with_capacity(hold + 1))
                },
                |(mut stream, mut held)| {
                    block_on(async {
                        while let Some(item) = stream.next().await {
                            if let Ok(packet) = item {
                                held.push_back(black_box(packet));
                                if held.len() > hold {
                                    held.pop_front();
                                }
                            }
                        }
                    });
                    (stream, held)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// IMIX packet count: 833 rounds of 12 packets.
const IMIX_N: usize = 9_996;
/// One IMIX round: seven 64-byte, four 576-byte and one 1500-byte packet.
const IMIX_ROUND: [usize; 12] = [64, 64, 64, 64, 64, 64, 64, 576, 576, 576, 576, 1500];

/// A finite device replaying [`IMIX_ROUND`] for [`IMIX_N`] packets, then
/// `Disconnected`.
struct ImixDevice {
    next: AtomicUsize,
}

impl ImixDevice {
    fn recv_now(&self, buf: &mut [u8]) -> Result<usize> {
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        if i >= IMIX_N {
            return Err(Error::Disconnected);
        }
        let n = IMIX_ROUND[i % IMIX_ROUND.len()];
        buf[..n].fill(0xA5);
        Ok(n)
    }
}

impl AsyncPacketIo for ImixDevice {
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
        ready(self.recv_now(buf))
    }

    fn send(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send {
        ready(Ok(buf.len()))
    }
}

fn stream_native_imix(c: &mut Criterion) {
    let round_bytes: usize = IMIX_ROUND.iter().sum();
    let mut group = c.benchmark_group("stream_native_imix");
    group.throughput(Throughput::ElementsAndBytes {
        elements: IMIX_N as u64,
        bytes: (IMIX_N / IMIX_ROUND.len() * round_bytes) as u64,
    });
    group.bench_function("7:4:1/1500", |b| {
        b.iter_batched(
            || {
                let device = Arc::new(ImixDevice {
                    next: AtomicUsize::new(0),
                });
                from_async_device(device, 1500).expect("valid buf_len")
            },
            |mut stream| {
                drain_pooled(&mut stream);
                stream
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    sync_recv_caller_buf,
    baseline_vec_native,
    baseline_vec_bridge,
    stream_native_pool,
    stream_native_pool_shared,
    stream_bridge_pool,
    stream_native_pool_hold,
    stream_native_imix
);
criterion_main!(benches);
