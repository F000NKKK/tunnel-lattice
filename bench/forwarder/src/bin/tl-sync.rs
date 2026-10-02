//! tunnel-lattice facade, blocking: `Handle::recv`/`Handle::send` on
//! `Handle` clones, one buffer of `recv_buffer_len()` bytes per thread.
//!
//! Built without any async feature, so the facade uses the backend's
//! blocking path. `--threads N` copy threads per direction (default 1).
//! `--ip1`/`--ip2` are ignored: the run script assigns the addresses.
//!
//! With `--offload`, both devices are opened with
//! `DeviceConfig::with_offload(true)` and must report
//! `Capability::SEGMENTATION_OFFLOAD`; the copy loop is unchanged, so
//! `Handle::recv` splits each kernel super-packet lazily and `Handle::send`
//! writes one packet per call. A blocking loop cannot tell whether another
//! packet is ready without waiting for it (the facade has no non-blocking
//! or batch receive), so it has nothing to batch: this variant measures the
//! receive-side split alone, and the async variants measure `send_batch`.

#[cfg(target_os = "linux")]
fn main() {
    use tunnel_lattice::{
        Capability, DeviceConfig, DeviceKind, Error, Handle, TunRsDevice, Tunnel,
    };
    use tunnel_lattice_bench_forwarder::cli::{Args, SkipCounter, fatal, ready};

    static SKIPPED: SkipCounter = SkipCounter::new();

    fn copy(
        src: &Handle<TunRsDevice>,
        dst: &Handle<TunRsDevice>,
        buf_len: usize,
        direction: &str,
    ) -> ! {
        let mut buf = vec![0u8; buf_len];
        loop {
            let len = match src.recv(&mut buf) {
                Ok(len) => len,
                Err(Error::BufferTooSmall) => {
                    SKIPPED.record(direction);
                    continue;
                }
                Err(error) => fatal(&format!("{direction} recv"), error),
            };
            if let Err(error) = dst.send(&buf[..len]) {
                fatal(&format!("{direction} send"), error);
            }
        }
    }

    let args = Args::from_env_or_exit();
    let tunnel = Tunnel::connect();
    let open = |name: &str| {
        let device = tunnel
            .open(
                DeviceConfig::new(DeviceKind::Tun)
                    .with_name(name)
                    .with_mtu(u32::from(args.mtu))
                    .with_offload(args.offload),
            )
            .unwrap_or_else(|error| fatal(&format!("open {name}"), error));
        if args.offload {
            if !device
                .capabilities()
                .contains(Capability::SEGMENTATION_OFFLOAD)
            {
                fatal(
                    &format!("open {name}"),
                    "offload requested but SEGMENTATION_OFFLOAD not reported",
                );
            }
            eprintln!("{name}: SEGMENTATION_OFFLOAD on");
        }
        device
    };
    let dev1 = open(&args.iface1);
    let dev2 = open(&args.iface2);
    let buf_len = dev1
        .snapshot()
        .unwrap_or_else(|error| fatal("snapshot", error))
        .recv_buffer_len();
    ready(&args);

    let mut threads = Vec::new();
    for _ in 0..args.threads.unwrap_or(1) {
        for (src, dst, direction) in [
            (dev1.clone(), dev2.clone(), "1->2"),
            (dev2.clone(), dev1.clone(), "2->1"),
        ] {
            threads.push(std::thread::spawn(move || {
                copy(&src, &dst, buf_len, direction)
            }));
        }
    }
    for thread in threads {
        if thread.join().is_err() {
            fatal("copy thread", "panicked");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    tunnel_lattice_bench_forwarder::cli::linux_only("tl-sync")
}
