//! tunnel-lattice facade, blocking: `Handle::recv`/`Handle::send` on
//! `Handle` clones, one buffer of `recv_buffer_len()` bytes per thread.
//!
//! Built without any async feature, so the facade uses the backend's
//! blocking path. `--threads N` copy threads per direction (default 1).
//! `--ip1`/`--ip2` are ignored: the run script assigns the addresses.

#[cfg(target_os = "linux")]
fn main() {
    use tunnel_lattice::{DeviceConfig, DeviceKind, Error, Handle, TunRsDevice, Tunnel};
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
        tunnel
            .open(
                DeviceConfig::new(DeviceKind::Tun)
                    .with_name(name)
                    .with_mtu(u32::from(args.mtu)),
            )
            .unwrap_or_else(|error| fatal(&format!("open {name}"), error))
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
