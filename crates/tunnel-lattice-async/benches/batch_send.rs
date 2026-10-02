//! Batch send cost on the provided per-packet path, over an in-memory sink.
//!
//! Every measured iteration sends [`N`] packets of [`PAYLOAD`] bytes to a
//! mock whose `send` only counts the packet and its bytes (no copy, no
//! system call), so the numbers are the batch API's own per-packet
//! overhead on a device without a batch override, not device throughput.
//!
//! Groups:
//!
//! - `send_per_packet`: `N` calls of `PacketIo::send`, the reference.
//! - `send_batch_sync/{1,8,64,128}`: the same `N` packets through the
//!   provided `PacketIo::send_batch`, in batches of that many packets (128
//!   is the most the tun-rs backend accepts per call), looping on a short
//!   batch the way a caller following the prefix contract must.
//! - `send_batch_async/{1,8,64,128}`: the same through the provided
//!   `AsyncPacketIo::send_batch`, all batches awaited inside one
//!   `block_on`.
//!
//! What a backend override gains (coalescing adjacent packets into fewer
//! native writes on an offload-framed Linux TUN queue), and the cost of
//! splitting received super-packets behind `recv`, both need a real
//! offload-framed device and live in a backend-private module, so neither
//! can be measured with a mock. The repository's iperf3 forwarder
//! benchmark measures them end to end. That `send_batch` allocates nothing
//! per packet is gated by the `alloc_count` test.
//!
//! Run with `cargo bench -p tunnel-lattice-async --bench batch_send`.

use std::future::{Future, ready};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures::executor::block_on;
use tunnel_lattice_async::{Error, Result};
use tunnel_lattice_platform::{AsyncPacketIo, PacketIo};

/// Packets sent per measured iteration.
const N: usize = 10_000;
/// Bytes per packet: a full-MTU TUN packet.
const PAYLOAD: usize = 1500;
/// Packets per `send_batch` call.
const BATCHES: [usize; 4] = [1, 8, 64, 128];
/// The largest batch.
const MAX_BATCH: usize = 128;

/// A device whose `send` accepts every packet and only counts it.
#[derive(Default)]
struct Sink {
    packets: AtomicU64,
    bytes: AtomicU64,
}

impl Sink {
    fn accept(&self, buf: &[u8]) -> Result<usize> {
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(black_box(buf).len())
    }
}

// The send benchmarks never receive; a receive reports a closed device.
impl PacketIo for Sink {
    fn recv(&self, _buf: &mut [u8]) -> Result<usize> {
        Err(Error::Disconnected)
    }

    fn send(&self, buf: &[u8]) -> Result<usize> {
        self.accept(buf)
    }
}

impl AsyncPacketIo for Sink {
    fn recv(&self, _buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
        ready(Err(Error::Disconnected))
    }

    fn send(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send {
        ready(self.accept(buf))
    }
}

fn throughput() -> Throughput {
    Throughput::ElementsAndBytes {
        elements: N as u64,
        bytes: (N * PAYLOAD) as u64,
    }
}

/// The reference: one `PacketIo::send` per packet.
fn send_per_packet(c: &mut Criterion) {
    let sink = Sink::default();
    let packet = vec![0xA5u8; PAYLOAD];
    let mut group = c.benchmark_group("send_per_packet");
    group.throughput(throughput());
    group.bench_function(BenchmarkId::from_parameter(PAYLOAD), |b| {
        b.iter(|| {
            for _ in 0..N {
                PacketIo::send(&sink, black_box(&packet)).expect("the sink accepts");
            }
        });
    });
    group.finish();
}

/// The provided `PacketIo::send_batch`, in batches of `batch` packets.
fn send_batch_sync(c: &mut Criterion) {
    let sink = Sink::default();
    let packet = vec![0xA5u8; PAYLOAD];
    let refs: [&[u8]; MAX_BATCH] = [&packet; MAX_BATCH];
    let mut group = c.benchmark_group("send_batch_sync");
    group.throughput(throughput());
    for batch in BATCHES {
        group.bench_function(BenchmarkId::from_parameter(batch), |b| {
            b.iter(|| {
                let mut sent = 0;
                while sent < N {
                    let packets = &refs[..batch.min(N - sent)];
                    sent +=
                        PacketIo::send_batch(&sink, black_box(packets)).expect("the sink accepts");
                }
            });
        });
    }
    group.finish();
}

/// The provided `AsyncPacketIo::send_batch`, in batches of `batch`
/// packets, awaited inside one `block_on` per iteration.
fn send_batch_async(c: &mut Criterion) {
    let sink = Sink::default();
    let packet = vec![0xA5u8; PAYLOAD];
    let refs: [&[u8]; MAX_BATCH] = [&packet; MAX_BATCH];
    let mut group = c.benchmark_group("send_batch_async");
    group.throughput(throughput());
    for batch in BATCHES {
        group.bench_function(BenchmarkId::from_parameter(batch), |b| {
            b.iter(|| {
                block_on(async {
                    let mut sent = 0;
                    while sent < N {
                        let packets = &refs[..batch.min(N - sent)];
                        sent += AsyncPacketIo::send_batch(&sink, black_box(packets))
                            .await
                            .expect("the sink accepts");
                    }
                });
            });
        });
    }
    group.finish();
}

criterion_group!(benches, send_per_packet, send_batch_sync, send_batch_async);
criterion_main!(benches);
