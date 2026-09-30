//! Packet receive-path throughput over in-memory mock devices.
//!
//! Every measured iteration receives [`support::N`] packets from a finite
//! mock (`N` packets, then `Disconnected`) and drains the stream to its
//! end. Building the device and stream, and dropping them afterwards, are
//! outside the timed routine (`iter_batched` with `PerIteration`).
//!
//! Timing bias: the thread-bridge mock is gated, and setup waits until the
//! worker thread is blocked on the gate, so the worker neither starts up
//! nor receives anything inside the timed routine until it opens the gate.
//! The timed window therefore covers all `N` packets on both the native and
//! the bridge path, plus one worker wake-up from the gate on the bridge path.
//! Without the gate the 0.4 bridge (an unbounded channel) could receive
//! every packet during setup and time only the consumer.
//!
//! Run with `cargo bench -p tunnel-lattice-async`. Numbers from mocks
//! measure this crate's own per-packet overhead (allocation, copies,
//! channel, wake-ups), not device throughput.

mod support;

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures::StreamExt;
use futures::executor::block_on;
use tunnel_lattice_platform::PacketIo;

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

criterion_group!(
    benches,
    sync_recv_caller_buf,
    baseline_vec_native,
    baseline_vec_bridge
);
criterion_main!(benches);
