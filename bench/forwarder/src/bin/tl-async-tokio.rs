//! tunnel-lattice facade on Tokio: `Handle::packet_stream` (each stream
//! with its own default-sized packet pool) to receive and
//! `Handle::send_async` to send, one task per direction.
//!
//! Runtime selection follows `--threads` exactly as the
//! `tunrs-async-tokio` baseline does. `--ip1`/`--ip2` are ignored: the run
//! script assigns the addresses.
//!
//! With `--offload`, both devices are opened with segmentation offload,
//! and each task sends every packet its stream already has ready with one
//! `Handle::send_batch_async` call instead of one `send_async` per packet.

#[cfg(target_os = "linux")]
fn main() {
    use tunnel_lattice_bench_forwarder::cli::{Args, fatal, ready};
    use tunnel_lattice_bench_forwarder::forward::{facade, open_facade, tokio_runtime};

    let args = Args::from_env_or_exit();
    tokio_runtime(args.threads).block_on(async {
        let (dev1, dev2, buf_len) = open_facade(&args);
        let stream = |device: &tunnel_lattice::Handle<_>| {
            device
                .packet_stream(buf_len)
                .unwrap_or_else(|error| fatal("packet_stream", error))
        };
        let (from1, from2) = (stream(&dev1), stream(&dev2));
        ready(&args);
        let offload = args.offload;
        let one = tokio::spawn(async move { facade(from1, dev2, "1->2", offload).await });
        let two = tokio::spawn(async move { facade(from2, dev1, "2->1", offload).await });
        let (one, two) = (one.await, two.await);
        if let Err(error) = one.and(two) {
            fatal("copy task", error);
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {
    tunnel_lattice_bench_forwarder::cli::linux_only("tl-async-tokio")
}
