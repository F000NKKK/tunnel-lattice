//! `PacketPool` construction cost.
//!
//! `pool_new/{buf_len}` times `PacketPool::with_buf_len(buf_len)` alone: the
//! zeroed slab allocation, the free list, and the eager mutex/condvar
//! initialisation. Dropping the pool (and freeing the slab) happens outside
//! the timed routine (`iter_batched` with `PerIteration`). The reported
//! number is plain time per construction; no throughput is attached.
//!
//! The lengths are a full-MTU TUN and TAP buffer, a jumbo TAP buffer, and
//! the largest TUN and TAP buffers the tun-rs backend can report (a 65535
//! MTU). With the default slot count those pools are about 192 KiB,
//! 1.1 MiB, 4 MiB and 3.9 MiB. Large zeroed allocations are usually served
//! by fresh zero pages, so this mostly measures the allocator's
//! large-allocation path, not a memset.
//!
//! Run with `cargo bench -p tunnel-lattice-async --bench pool`.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use tunnel_lattice_async::PacketPool;

const BUF_LENS: [usize; 4] = [1500, 9018, 65_535, 65_553];

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

criterion_group!(benches, pool_new);
criterion_main!(benches);
