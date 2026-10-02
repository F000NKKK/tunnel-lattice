//! tunnel-lattice facade on async-io: `Handle::packet_stream` (each stream
//! with its own default-sized packet pool) to receive and
//! `Handle::send_async` to send, one thread per direction running
//! `async_io::block_on`, as the `tunrs-async-asyncio` baseline does.
//!
//! `--threads` and `--ip1`/`--ip2` are ignored: the run script assigns the
//! addresses.
//!
//! With `--offload`, both devices are opened with segmentation offload,
//! and each thread sends every packet its stream already has ready with
//! one `Handle::send_batch_async` call instead of one `send_async` per
//! packet.

#[cfg(target_os = "linux")]
fn main() {
    use tunnel_lattice_bench_forwarder::cli::{Args, fatal, ready};
    use tunnel_lattice_bench_forwarder::forward::{facade, open_facade};

    let args = Args::from_env_or_exit();
    let (dev1, dev2, buf_len) = open_facade(&args);
    let stream = |device: &tunnel_lattice::Handle<_>| {
        device
            .packet_stream(buf_len)
            .unwrap_or_else(|error| fatal("packet_stream", error))
    };
    let (from1, from2) = (stream(&dev1), stream(&dev2));
    ready(&args);
    let threads =
        [(from1, dev2, "1->2"), (from2, dev1, "2->1")].map(|(packets, dst, direction)| {
            let offload = args.offload;
            std::thread::spawn(move || async_io::block_on(facade(packets, dst, direction, offload)))
        });
    for thread in threads {
        if thread.join().is_err() {
            fatal("copy thread", "panicked");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    tunnel_lattice_bench_forwarder::cli::linux_only("tl-async-asyncio")
}
