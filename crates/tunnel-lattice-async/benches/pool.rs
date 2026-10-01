//! `PacketPool` construction cost and pool overhead on the stream path.
//!
//! - `pool_new/{buf_len}` times `PacketPool::with_buf_len(buf_len)` alone:
//!   the zeroed slab allocation, the slot flags, and the eager mutex/condvar
//!   initialisation. Dropping the pool (and freeing the slab) happens
//!   outside the timed routine (`iter_batched` with `PerIteration`). The
//!   reported number is plain time per construction; no throughput is
//!   attached.
//! - `pool_new_touch/{buf_len}` times the same construction plus a native
//!   stream that receives one packet into every slot (the mock fills the
//!   whole receive buffer, and the consumer takes exactly one packet per
//!   slot and holds them all, so each slot is written once). This is what
//!   first use of a fresh pool costs,
//!   including faulting in pages the allocator handed out lazily. The
//!   throughput is the slab's slot bytes (`slots * stride`).
//! - `stream_pool_overhead_zero_copy` drains `N` packets from a native
//!   stream over a mock whose `recv` writes nothing, and
//!   `stream_reuse_box_control_zero_copy` does the same with a hand-written
//!   stream that receives into one reused boxed buffer and yields only the
//!   length. The difference between the two is an upper bound on what the
//!   pool (acquire, view, release) costs per packet.
//! - `stream_shared_pool_contended/{T}` runs `T` threads, each draining its
//!   own native stream of `N` packets over a zero-work mock, all on one
//!   shared pool. It measures stream-level contention on the pool; mock and
//!   executor costs are included, thread start-up is not (the threads wait
//!   at a barrier the timed routine releases).
//! - `advance_truncate` times `PacketBuf::advance` plus `truncate` on one
//!   received packet (plain time per call pair).
//!
//! The lengths are a full-MTU TUN and TAP buffer, a jumbo TAP buffer, and
//! the largest TUN and TAP buffers the tun-rs backend can report (a 65535
//! MTU). With the default slot count those pools are about 192 KiB,
//! 1.1 MiB, 4 MiB and 3.9 MiB. Large zeroed allocations are usually served
//! by fresh zero pages, so `pool_new` mostly measures the allocator's
//! large-allocation path, not a memset.
//!
//! Run with `cargo bench -p tunnel-lattice-async --bench pool`.

use std::future::{Future, ready};
use std::hint::black_box;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures::executor::block_on;
use futures::{Stream, StreamExt};
use tunnel_lattice_async::{
    Error, PacketBuf, PacketPool, PacketStream, Result, from_async_device,
    from_async_device_with_pool,
};
use tunnel_lattice_platform::AsyncPacketIo;

const BUF_LENS: [usize; 4] = [1500, 9018, 65_535, 65_553];

/// Packets per stream in the streaming groups.
const N: usize = 10_000;

/// Payload and buffer length of the zero-copy groups.
const PAYLOAD: usize = 1500;

/// A finite mock: `remaining` packets, then `Disconnected`. With `fill`, it
/// writes the whole receive buffer and reports its length; otherwise it
/// writes nothing and reports [`PAYLOAD`] bytes (zero work).
struct Mock {
    remaining: AtomicUsize,
    fill: bool,
}

impl Mock {
    fn zero_copy(count: usize) -> Arc<Self> {
        Arc::new(Self {
            remaining: AtomicUsize::new(count),
            fill: false,
        })
    }

    fn filling(count: usize) -> Arc<Self> {
        Arc::new(Self {
            remaining: AtomicUsize::new(count),
            fill: true,
        })
    }

    fn recv_now(&self, buf: &mut [u8]) -> Result<usize> {
        let took = self
            .remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok();
        if !took {
            return Err(Error::Disconnected);
        }
        if self.fill {
            buf.fill(0xA5);
            Ok(buf.len())
        } else {
            Ok(PAYLOAD.min(buf.len()))
        }
    }
}

impl AsyncPacketIo for Mock {
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Result<usize>> + Send {
        ready(self.recv_now(buf))
    }

    fn send(&self, buf: &[u8]) -> impl Future<Output = Result<usize>> + Send {
        ready(Ok(buf.len()))
    }
}

fn streamed(packets: usize) -> Throughput {
    Throughput::ElementsAndBytes {
        elements: packets as u64,
        bytes: (packets * PAYLOAD) as u64,
    }
}

fn drain(stream: &mut PacketStream) {
    block_on(async {
        while let Some(item) = stream.next().await {
            if let Ok(packet) = item {
                black_box(&packet);
            }
        }
    });
}

fn pool_new(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_new");
    for buf_len in BUF_LENS {
        group.bench_function(BenchmarkId::from_parameter(buf_len), |b| {
            b.iter_batched(
                || (),
                |()| PacketPool::with_buf_len(black_box(buf_len)).expect("valid buf_len"),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn pool_new_touch(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_new_touch");
    for buf_len in BUF_LENS {
        let slots = PacketPool::with_buf_len(buf_len)
            .expect("valid buf_len")
            .slots();
        let stride = (buf_len + 1).next_multiple_of(64);
        group.throughput(Throughput::Bytes((slots * stride) as u64));
        group.bench_function(BenchmarkId::from_parameter(buf_len), |b| {
            b.iter_batched(
                || (Mock::filling(slots), Vec::with_capacity(slots)),
                |(device, mut held)| {
                    let pool = PacketPool::with_buf_len(black_box(buf_len)).expect("valid buf_len");
                    let mut stream = from_async_device_with_pool(device, pool);
                    // Exactly `slots` items: with every slot held, a further
                    // `next` would wait for a free slot forever.
                    block_on(async {
                        for _ in 0..slots {
                            let packet = stream
                                .next()
                                .await
                                .expect("one item per slot")
                                .expect("a packet");
                            held.push(packet);
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

fn stream_pool_overhead_zero_copy(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_pool_overhead_zero_copy");
    group.throughput(streamed(N));
    group.bench_function(BenchmarkId::from_parameter(PAYLOAD), |b| {
        b.iter_batched(
            || from_async_device(Mock::zero_copy(N), PAYLOAD).expect("valid buf_len"),
            |mut stream| {
                drain(&mut stream);
                stream
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

/// The control: the same native `recv` loop into one reused boxed buffer,
/// yielding only the received length.
type ControlStream = Pin<Box<dyn Stream<Item = Result<usize>> + Send>>;

fn control_stream(device: Arc<Mock>, buf_len: usize) -> ControlStream {
    let buf = vec![0u8; buf_len].into_boxed_slice();
    Box::pin(futures::stream::unfold(
        Some((device, buf)),
        |state| async move {
            let (device, mut buf) = state?;
            match device.recv(&mut buf).await {
                Ok(n) => Some((Ok(n), Some((device, buf)))),
                Err(err) => Some((Err(err), None)),
            }
        },
    ))
}

fn stream_reuse_box_control_zero_copy(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_reuse_box_control_zero_copy");
    group.throughput(streamed(N));
    group.bench_function(BenchmarkId::from_parameter(PAYLOAD), |b| {
        b.iter_batched(
            || control_stream(Mock::zero_copy(N), PAYLOAD),
            |mut stream| {
                block_on(async {
                    while let Some(item) = stream.next().await {
                        if let Ok(n) = item {
                            black_box(n);
                        }
                    }
                });
                stream
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn stream_shared_pool_contended(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_shared_pool_contended");
    for threads in [2usize, 4, 8] {
        group.throughput(streamed(threads * N));
        group.bench_function(BenchmarkId::from_parameter(format!("T={threads}")), |b| {
            b.iter_batched(
                || {
                    let pool = PacketPool::with_buf_len(PAYLOAD).expect("valid buf_len");
                    let start = Arc::new(Barrier::new(threads + 1));
                    let workers: Vec<_> = (0..threads)
                        .map(|_| {
                            let mut stream =
                                from_async_device_with_pool(Mock::zero_copy(N), pool.clone());
                            let start = Arc::clone(&start);
                            thread::spawn(move || {
                                start.wait();
                                drain(&mut stream);
                                stream
                            })
                        })
                        .collect();
                    (start, workers)
                },
                |(start, workers)| {
                    start.wait();
                    workers
                        .into_iter()
                        .map(|worker| worker.join().expect("worker panicked"))
                        .collect::<Vec<_>>()
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn advance_truncate(c: &mut Criterion) {
    // An endless zero-work stream; each batch holds at most 64 packets, well
    // below the default pool's 128 slots, so `next` never waits.
    let mut stream =
        from_async_device(Mock::zero_copy(usize::MAX), PAYLOAD).expect("valid buf_len");
    let mut next = move || -> PacketBuf {
        block_on(stream.next())
            .expect("an endless stream")
            .expect("a packet")
    };
    c.bench_function("advance_truncate", |b| {
        b.iter_batched(
            &mut next,
            |mut packet| {
                packet.advance(black_box(14));
                packet.truncate(black_box(1000));
                black_box(packet.len());
                packet
            },
            BatchSize::NumIterations(64),
        );
    });
}

criterion_group!(
    benches,
    pool_new,
    pool_new_touch,
    stream_pool_overhead_zero_copy,
    stream_reuse_box_control_zero_copy,
    stream_shared_pool_contended,
    advance_truncate
);
criterion_main!(benches);
