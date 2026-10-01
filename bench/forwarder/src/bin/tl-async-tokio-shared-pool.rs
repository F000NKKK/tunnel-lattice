//! tunnel-lattice facade on Tokio with one packet pool shared by both
//! directions: `PacketPool::new(256, recv_buffer_len)` passed to
//! `Handle::packet_stream_with_pool` for each device, `Handle::send_async`
//! to send.
//!
//! Otherwise identical to `tl-async-tokio`. `--ip1`/`--ip2` are ignored:
//! the run script assigns the addresses.

#[cfg(target_os = "linux")]
fn main() {
    use tunnel_lattice::PacketPool;
    use tunnel_lattice_bench_forwarder::cli::{Args, fatal, ready};
    use tunnel_lattice_bench_forwarder::forward::{facade, open_facade, tokio_runtime};

    /// Slots in the shared pool (128 per direction).
    const SLOTS: usize = 256;

    let args = Args::from_env_or_exit();
    tokio_runtime(args.threads).block_on(async {
        let (dev1, dev2, buf_len) = open_facade(&args);
        let pool =
            PacketPool::new(SLOTS, buf_len).unwrap_or_else(|error| fatal("PacketPool::new", error));
        let from1 = dev1.packet_stream_with_pool(pool.clone());
        let from2 = dev2.packet_stream_with_pool(pool);
        ready(&args);
        let one = tokio::spawn(async move { facade(from1, dev2, "1->2").await });
        let two = tokio::spawn(async move { facade(from2, dev1, "2->1").await });
        let (one, two) = (one.await, two.await);
        if let Err(error) = one.and(two) {
            fatal("copy task", error);
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {
    tunnel_lattice_bench_forwarder::cli::linux_only("tl-async-tokio-shared-pool")
}
