//! Privileged real-device packet benchmark: latency and echo throughput of
//! each packet path on a real TUN and TAP device.
//!
//! **Opt-in.** Does nothing unless `TUNNEL_LATTICE_PRIVILEGED_BENCH=1` is
//! set and the binary runs as a benchmark (`--bench`, which `cargo bench`
//! passes), so an ordinary `cargo bench` or `cargo test --all-targets`
//! never opens a device. It needs root (Linux, macOS) or Administrator
//! (Windows; plus `wintun.dll` on the `PATH` for TUN and the tap-windows6
//! driver for TAP). Build unprivileged and run only the built binary with
//! privilege, for example on Linux:
//!
//! ```sh
//! cargo bench -p tunnel-lattice --bench device --no-run
//! sudo env TUNNEL_LATTICE_PRIVILEGED_BENCH=1 target/release/deps/device-<hash> --bench
//! ```
//!
//! **Method.** For each packet path and device kind, the benchmark opens a
//! device, gives it the address `198.18.N.1/24` (the RFC 2544 benchmarking
//! range), and starts a reflector on the path under test. A UDP socket
//! bound to `198.18.N.1` sends datagrams to `198.18.N.2`, which the host
//! routes into the device; the reflector reads each one, swaps source and
//! destination in place (answering ARP for the peer on TAP), and writes it
//! back, so the host delivers it to the socket. Per payload size it then
//! measures round-trip latency with one datagram in flight and echo
//! throughput with a window of datagrams in flight.
//!
//! Paths, by build:
//!
//! | Path | default | `async-io` | `tokio` |
//! |---|---|---|---|
//! | `sync`: `Handle::recv` / `Handle::send` on one thread | yes | yes (blocks on the async device) | yes (blocks on the async device, runtime entered) |
//! | `native-async`: `Handle::packet_stream` + `Handle::send_async` | — | yes (`futures` executor) | yes (a task on a multi-thread runtime) |
//! | `thread-bridge`: `tunnel_lattice_async::from_device_with_pool` + `PacketIo::send` | yes | yes | skipped: the bridge's worker thread has no Tokio runtime, which this build's blocking `recv` needs |
//!
//! **Teardown.** Each device is dropped when its reflector stops (a stop
//! datagram, or, if that does not arrive, the release steps in `host.rs`).
//! Host state that outlives the process (a macOS `feth` pair, the Windows
//! firewall rule) has a guard that runs on every unwinding exit path.
//!
//! **Output.** A Markdown table on stdout; with
//! `TUNNEL_LATTICE_BENCH_OUT=<dir>`, also `device-<os>-<build>.json` and
//! `.md` in that directory. Exits non-zero if any scenario failed. Other
//! knobs: `TUNNEL_LATTICE_BENCH_RTTS` (round trips per latency figure,
//! default 2000), `TUNNEL_LATTICE_BENCH_SECS` (seconds per throughput
//! figure, default 3), `TUNNEL_LATTICE_BENCH_WINDOW` (datagrams in flight,
//! default 64), `TUNNEL_LATTICE_BENCH_KINDS` (`tun`, `tap` or `tun,tap`,
//! the default).

mod host;
mod packet;
mod report;
mod socket;

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(feature = "async")]
use futures::StreamExt;
use tunnel_lattice::{
    Capability, DeviceConfig, DeviceKind, DeviceObserver, DeviceProvider, Error, PacketIo,
    TunRsBackend, TunRsDevice, Tunnel,
};

use packet::{Action, Flow, Framing};
use report::{Meta, Outcome, Scenario};

/// The opt-in variable; the benchmark runs only when it is `1`.
const GUARD: &str = "TUNNEL_LATTICE_PRIVILEGED_BENCH";

/// The build's feature set.
const BUILD: &str = if cfg!(feature = "tokio") {
    "tokio"
} else if cfg!(feature = "async-io") {
    "async-io"
} else {
    "default"
};

const MTU: u32 = 1500;
const PEER_PORT: u16 = 7;
const PAYLOADS: [usize; 2] = [64, 1400];
const LATENCY_WARMUP: usize = 200;

/// Settings read from the environment.
struct Config {
    rtts: usize,
    secs: f64,
    window: u64,
    kinds: Vec<DeviceKind>,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        fn var<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
            match std::env::var(name) {
                Ok(value) => value
                    .parse()
                    .map_err(|_| format!("{name}={value:?} is not valid")),
                Err(_) => Ok(default),
            }
        }
        let kinds =
            std::env::var("TUNNEL_LATTICE_BENCH_KINDS").unwrap_or_else(|_| "tun,tap".into());
        let kinds = kinds
            .split(',')
            .map(|kind| match kind.trim() {
                "tun" => Ok(DeviceKind::Tun),
                "tap" => Ok(DeviceKind::Tap),
                other => Err(format!("unknown device kind {other:?}")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let config = Self {
            rtts: var("TUNNEL_LATTICE_BENCH_RTTS", 2000)?,
            secs: var("TUNNEL_LATTICE_BENCH_SECS", 3.0)?,
            window: var("TUNNEL_LATTICE_BENCH_WINDOW", 64)?,
            kinds,
        };
        if config.rtts == 0 || config.window == 0 || !config.secs.is_finite() || config.secs <= 0.0
        {
            return Err("RTTS, SECS and WINDOW must be positive".into());
        }
        Ok(config)
    }
}

/// A packet path under test.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    Sync,
    NativeAsync,
    ThreadBridge,
}

impl Path {
    fn id(self) -> &'static str {
        match self {
            Path::Sync => "sync",
            Path::NativeAsync => "native-async",
            Path::ThreadBridge => "thread-bridge",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Path::Sync if cfg!(feature = "async") => {
                "sync Handle::recv/send (blocking on the async device)"
            }
            Path::Sync => "sync Handle::recv/send",
            Path::NativeAsync if cfg!(feature = "tokio") => {
                "native async packet_stream + send_async (Tokio task)"
            }
            Path::NativeAsync => "native async packet_stream + send_async (async-io)",
            Path::ThreadBridge => "thread bridge from_device + PacketIo::send",
        }
    }

    /// The paths this build measures (or reports as skipped).
    fn all() -> Vec<Path> {
        if cfg!(feature = "async") {
            vec![Path::Sync, Path::NativeAsync, Path::ThreadBridge]
        } else {
            vec![Path::Sync, Path::ThreadBridge]
        }
    }

    /// Why this path cannot run in this build, if it cannot.
    fn skip_reason(self) -> Option<&'static str> {
        (self == Path::ThreadBridge && cfg!(feature = "tokio")).then_some(
            "the bridge worker thread has no Tokio runtime, which the tokio build's blocking recv needs",
        )
    }
}

/// What a reflector did; reported on stderr.
#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    reflected: u64,
    replies: u64,
    ignored: u64,
    too_small: u64,
}

impl Counters {
    /// Records `action`; `true` once the loop should stop.
    fn record(&mut self, action: Action) -> bool {
        match action {
            Action::Reflect => self.reflected += 1,
            Action::Reply(_) => self.replies += 1,
            Action::Ignore => self.ignored += 1,
            Action::Stop => return true,
        }
        false
    }
}

type ReflectResult = Result<Counters, String>;

/// The reflector loop over blocking receive and send functions.
fn reflect_blocking(
    framing: Framing,
    flow: Flow,
    buf_len: usize,
    mut recv: impl FnMut(&mut [u8]) -> tunnel_lattice::Result<usize>,
    mut send: impl FnMut(&[u8]) -> tunnel_lattice::Result<usize>,
) -> ReflectResult {
    let mut buf = vec![0u8; buf_len];
    let mut reply = [0u8; packet::MIN_ETHERNET_FRAME];
    let mut counters = Counters::default();
    loop {
        let len = match recv(&mut buf) {
            Ok(len) => len,
            Err(Error::BufferTooSmall) => {
                counters.too_small += 1;
                continue;
            }
            Err(err) => return Err(format!("recv: {err}")),
        };
        let action = packet::reflect(framing, flow, &mut buf[..len], &mut reply);
        let out = match action {
            Action::Reflect => &buf[..len],
            Action::Reply(reply_len) => &reply[..reply_len],
            _ => &[][..],
        };
        if !out.is_empty() {
            send(out).map_err(|err| format!("send: {err}"))?;
        }
        if counters.record(action) {
            return Ok(counters);
        }
    }
}

/// The reflector loop over `Handle::packet_stream` and `Handle::send_async`.
#[cfg(feature = "async")]
async fn reflect_stream(
    handle: tunnel_lattice::Handle<TunRsDevice>,
    framing: Framing,
    flow: Flow,
    buf_len: usize,
) -> ReflectResult {
    let mut stream = handle
        .packet_stream(buf_len)
        .map_err(|err| format!("packet_stream: {err}"))?;
    let mut reply = [0u8; packet::MIN_ETHERNET_FRAME];
    let mut counters = Counters::default();
    let result = loop {
        let mut item = match stream.next().await {
            None => break Err("the stream ended".to_owned()),
            Some(Ok(item)) => item,
            Some(Err(Error::BufferTooSmall)) => {
                counters.too_small += 1;
                continue;
            }
            Some(Err(err)) => break Err(format!("recv: {err}")),
        };
        let action = packet::reflect(framing, flow, &mut item, &mut reply);
        let sent = match action {
            Action::Reflect => Some(handle.send_async(&item).await),
            Action::Reply(reply_len) => Some(handle.send_async(&reply[..reply_len]).await),
            _ => None,
        };
        if let Some(Err(err)) = sent {
            break Err(format!("send_async: {err}"));
        }
        drop(item);
        if counters.record(action) {
            break Ok(counters);
        }
    };
    drop(stream);
    drop(handle);
    result
}

/// The reflector loop over the thread bridge: a `PacketStream` from
/// `tunnel_lattice_async::from_device_with_pool`, driven by a blocking
/// executor, and blocking `PacketIo::send`.
fn reflect_bridge(
    device: Arc<TunRsDevice>,
    framing: Framing,
    flow: Flow,
    buf_len: usize,
) -> ReflectResult {
    use futures::StreamExt as _;

    let pool = tunnel_lattice_async::PacketPool::with_buf_len(buf_len)
        .map_err(|err| format!("PacketPool: {err}"))?;
    let mut stream = tunnel_lattice_async::from_device_with_pool(Arc::clone(&device), pool);
    let mut reply = [0u8; packet::MIN_ETHERNET_FRAME];
    let mut counters = Counters::default();
    let result = loop {
        let mut item = match futures::executor::block_on(stream.next()) {
            None => break Err("the stream ended".to_owned()),
            Some(Ok(item)) => item,
            Some(Err(Error::BufferTooSmall)) => {
                counters.too_small += 1;
                continue;
            }
            Some(Err(err)) => break Err(format!("recv: {err}")),
        };
        let action = packet::reflect(framing, flow, &mut item, &mut reply);
        let sent = match action {
            Action::Reflect => Some(PacketIo::send(&*device, &item)),
            Action::Reply(reply_len) => Some(PacketIo::send(&*device, &reply[..reply_len])),
            _ => None,
        };
        if let Some(Err(err)) = sent {
            break Err(format!("send: {err}"));
        }
        drop(item);
        if counters.record(action) {
            break Ok(counters);
        }
    };
    // The worker may still be waiting in `recv` and holds its own clone of
    // the device; `stop` sends datagrams until it lets go.
    drop(stream);
    drop(device);
    result
}

/// A started reflector on a freshly opened device.
struct Running {
    name: String,
    done: Receiver<ReflectResult>,
    thread: JoinHandle<()>,
    /// The thread bridge's device, which its worker thread can keep alive
    /// after the reflector returned.
    bridge_device: Option<Weak<TunRsDevice>>,
}

/// Shared runtime state of one benchmark run.
struct Runtime {
    #[cfg(feature = "tokio")]
    tokio: tokio::runtime::Runtime,
}

impl Runtime {
    fn new() -> Result<Self, String> {
        Ok(Self {
            #[cfg(feature = "tokio")]
            tokio: tokio::runtime::Builder::new_multi_thread()
                .enable_io()
                .build()
                .map_err(|err| format!("build the Tokio runtime: {err}"))?,
        })
    }
}

fn framing(kind: DeviceKind) -> Framing {
    if kind == DeviceKind::Tap {
        Framing::Ethernet
    } else {
        Framing::Ip
    }
}

/// Opens a device of `kind` and starts the reflector of `path` on it.
fn start(runtime: &Runtime, path: Path, kind: DeviceKind, flow: Flow) -> Result<Running, String> {
    let config = DeviceConfig::new(kind).with_mtu(MTU);
    let framing = framing(kind);
    let (tx, done) = channel();
    #[cfg(feature = "tokio")]
    let _entered = runtime.tokio.enter();
    #[cfg(not(feature = "tokio"))]
    let _ = runtime;
    match path {
        Path::Sync | Path::NativeAsync => {
            let handle = Tunnel::connect()
                .open(config)
                .map_err(|err| format!("open: {err}"))?;
            let snapshot = handle
                .snapshot()
                .map_err(|err| format!("snapshot: {err}"))?;
            let buf_len = snapshot.recv_buffer_len();
            #[cfg(feature = "tokio")]
            let tokio = runtime.tokio.handle().clone();
            let thread = match path {
                Path::Sync => thread::spawn(move || {
                    #[cfg(feature = "tokio")]
                    let _entered = tokio.enter();
                    let result = reflect_blocking(
                        framing,
                        flow,
                        buf_len,
                        |buf| handle.recv(buf),
                        |buf| handle.send(buf),
                    );
                    drop(handle);
                    let _ = tx.send(result);
                }),
                #[cfg(feature = "tokio")]
                _ => thread::spawn(move || {
                    let task = tokio.spawn(reflect_stream(handle, framing, flow, buf_len));
                    let result = tokio
                        .block_on(task)
                        .unwrap_or_else(|err| Err(format!("the reflector task failed: {err}")));
                    let _ = tx.send(result);
                }),
                #[cfg(all(feature = "async", not(feature = "tokio")))]
                _ => thread::spawn(move || {
                    let result =
                        futures::executor::block_on(reflect_stream(handle, framing, flow, buf_len));
                    let _ = tx.send(result);
                }),
                #[cfg(not(feature = "async"))]
                _ => return Err("native async needs the async-io or tokio feature".into()),
            };
            Ok(Running {
                name: snapshot.name,
                done,
                thread,
                bridge_device: None,
            })
        }
        Path::ThreadBridge => {
            let device = Arc::new(
                TunRsBackend::new()
                    .open(config)
                    .map_err(|err| format!("open: {err}"))?,
            );
            let snapshot = device
                .snapshot()
                .map_err(|err| format!("snapshot: {err}"))?;
            let bridge_device = Some(Arc::downgrade(&device));
            let buf_len = snapshot.recv_buffer_len();
            let thread = thread::spawn(move || {
                let _ = tx.send(reflect_bridge(device, framing, flow, buf_len));
            });
            Ok(Running {
                name: snapshot.name,
                done,
                thread,
                bridge_device,
            })
        }
    }
}

/// Stops the reflector and waits until its device is closed: stop
/// datagrams first, then the host release steps. An error means the
/// reflector failed, or did not stop and the device is left to process
/// exit.
fn stop(
    running: Running,
    socket: Option<&UdpSocket>,
    kind: DeviceKind,
    feth_peer: Option<&str>,
) -> ReflectResult {
    let wait = |timeout: Duration, nudge: &dyn Fn()| -> Option<ReflectResult> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            nudge();
            match running.done.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => return Some(result),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Some(Err("the reflector thread panicked".into()));
                }
            }
        }
        None
    };
    let mut released = false;
    let result = match wait(Duration::from_secs(5), &|| {
        if let Some(socket) = socket {
            socket::send_stop(socket);
        }
    }) {
        Some(result) => result,
        None => {
            host::release(kind, &running.name, feth_peer);
            released = true;
            match wait(Duration::from_secs(10), &|| {}) {
                Some(Ok(_)) => {
                    Err("the reflector did not stop until its device was released".into())
                }
                Some(Err(err)) => Err(format!("the reflector did not stop; after release: {err}")),
                None => {
                    return Err(
                        "the reflector did not stop even after its device was released; \
                                the device is left to process exit"
                            .into(),
                    );
                }
            }
        }
    };
    let _ = running.thread.join();
    if let Some(device) = running.bridge_device {
        let deadline = Instant::now() + Duration::from_secs(5);
        while device.strong_count() > 0 && Instant::now() < deadline {
            if let Some(socket) = socket {
                socket::send_wake(socket);
            }
            thread::sleep(Duration::from_millis(50));
        }
        if device.strong_count() > 0 && !released {
            host::release(kind, &running.name, feth_peer);
            let deadline = Instant::now() + Duration::from_secs(10);
            while device.strong_count() > 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
        }
        if device.strong_count() > 0 {
            return Err("the bridge worker kept the device open".into());
        }
    }
    result
}

fn kind_id(kind: DeviceKind) -> &'static str {
    if kind == DeviceKind::Tap {
        "tap"
    } else {
        "tun"
    }
}

fn wire_len(kind: DeviceKind, payload: usize) -> usize {
    payload.max(8)
        + packet::IPV4_UDP_HEADERS
        + if kind == DeviceKind::Tap {
            packet::ETHERNET_HEADER
        } else {
            0
        }
}

fn scenario(path: Path, kind: DeviceKind, payload: usize, outcome: Outcome) -> Scenario {
    Scenario {
        path: path.id().to_owned(),
        label: path.label().to_owned(),
        kind: kind_id(kind).to_owned(),
        payload,
        wire_len: if payload == 0 {
            0
        } else {
            wire_len(kind, payload)
        },
        latency: None,
        throughput: None,
        outcome,
    }
}

/// Measures every payload size for one path on one device kind.
fn run_case(
    runtime: &Runtime,
    config: &Config,
    path: Path,
    kind: DeviceKind,
    subnet: u8,
) -> Vec<Scenario> {
    let flow = Flow {
        local: Ipv4Addr::new(198, 18, subnet, 1),
        peer: Ipv4Addr::new(198, 18, subnet, 2),
    };
    let failed = |reason: String| vec![scenario(path, kind, 0, Outcome::Failed(reason))];
    let running = match start(runtime, path, kind, flow) {
        Ok(running) => running,
        Err(err) => return failed(err),
    };
    eprintln!(
        "device-bench: {} on {} {} ({}/24)",
        path.id(),
        kind_id(kind),
        running.name,
        flow.local
    );
    let configured = match host::configure(kind, &running.name, flow.local, flow.peer) {
        Ok(configured) => configured,
        Err(err) => {
            let stopped = stop(running, None, kind, None);
            return failed(format!("configure: {err}; stop: {stopped:?}"));
        }
    };
    let feth_peer = configured.feth_peer.clone();
    let socket = socket::connect(
        SocketAddrV4::new(flow.local, 0),
        SocketAddrV4::new(flow.peer, PEER_PORT),
    );
    let mut scenarios = Vec::new();
    let socket = match socket.and_then(|socket| {
        socket::wait_ready(&socket, Duration::from_secs(30))?;
        Ok(socket)
    }) {
        Ok(socket) => Some(socket),
        Err(err) => {
            scenarios.push(scenario(path, kind, 0, Outcome::Failed(err)));
            None
        }
    };
    if let Some(socket) = &socket {
        for payload in PAYLOADS {
            let mut row = scenario(path, kind, payload, Outcome::Ok);
            let measured =
                socket::latency(socket, payload, LATENCY_WARMUP, config.rtts).and_then(|latency| {
                    row.latency = Some(latency);
                    socket::throughput(socket, payload, row.wire_len, config.secs, config.window)
                });
            match measured {
                Ok(throughput) => row.throughput = Some(throughput),
                Err(err) => row.outcome = Outcome::Failed(err),
            }
            scenarios.push(row);
        }
    }
    let stopped = stop(running, socket.as_ref(), kind, feth_peer.as_deref());
    drop(socket);
    drop(configured);
    match stopped {
        Ok(counters) => eprintln!("device-bench:   reflector {counters:?}"),
        Err(err) => {
            for row in &mut scenarios {
                if row.outcome == Outcome::Ok {
                    row.outcome = Outcome::Failed(format!("reflector: {err}"));
                }
            }
            if scenarios.is_empty() {
                scenarios.push(scenario(
                    path,
                    kind,
                    0,
                    Outcome::Failed(format!("reflector: {err}")),
                ));
            }
        }
    }
    scenarios
}

fn meta(config: &Config) -> Meta {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let runner = match (env("ImageOS"), env("ImageVersion")) {
        (os, _) if os.is_empty() => String::new(),
        (os, version) => format!("GitHub Actions {os} {version}").trim().to_owned(),
    };
    Meta {
        created_utc: report::utc_rfc3339(now),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        build: BUILD.to_owned(),
        cpus: thread::available_parallelism().map_or(0, usize::from),
        cpu_model: host::cpu_model(),
        kernel: host::kernel(),
        runner,
        git_sha: env("GITHUB_SHA"),
        crate_version: env!("CARGO_PKG_VERSION").to_owned(),
        rtts: config.rtts,
        secs: config.secs,
        window: config.window,
    }
}

fn run(config: &Config) -> Result<Vec<Scenario>, String> {
    let runtime = Runtime::new()?;
    let firewall = match host::allow_inbound("198.18.0.0/16") {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("device-bench: warning: could not add the firewall rule: {err}");
            None
        }
    };
    let host_caps = Tunnel::connect().capabilities();
    let mut scenarios = Vec::new();
    let mut subnet = 10u8;
    for &kind in &config.kinds {
        for path in Path::all() {
            if let Some(reason) = path.skip_reason() {
                scenarios.push(scenario(path, kind, 0, Outcome::Skipped(reason.into())));
                continue;
            }
            if kind == DeviceKind::Tap && !host_caps.contains(Capability::TAP_DEVICES) {
                scenarios.push(scenario(
                    path,
                    kind,
                    0,
                    Outcome::Skipped(
                        "the host reports no TAP support (driver not installed)".into(),
                    ),
                ));
                continue;
            }
            subnet += 1;
            let case = catch_unwind(AssertUnwindSafe(|| {
                run_case(&runtime, config, path, kind, subnet)
            }));
            scenarios.extend(case.unwrap_or_else(|panic| {
                let reason = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "panicked".to_owned());
                vec![scenario(
                    path,
                    kind,
                    0,
                    Outcome::Failed(format!("panic: {reason}")),
                )]
            }));
        }
    }
    drop(firewall);
    Ok(scenarios)
}

fn main() {
    if std::env::var(GUARD).as_deref() != Ok("1") {
        println!(
            "device bench: skipped; set {GUARD}=1 and run with root/Administrator to measure real devices (see CONTRIBUTING.md)"
        );
        return;
    }
    if !std::env::args().any(|arg| arg == "--bench") {
        println!("device bench: skipped outside `cargo bench` (no --bench argument)");
        return;
    }
    if !cfg!(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows"
    )) {
        println!("device bench: skipped; only Linux, macOS and Windows are supported");
        return;
    }
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("device bench: {err}");
            std::process::exit(2);
        }
    };
    let meta = meta(&config);
    let scenarios = match run(&config) {
        Ok(scenarios) => scenarios,
        Err(err) => {
            eprintln!("device bench: {err}");
            std::process::exit(1);
        }
    };
    let markdown = report::to_markdown(&meta, &scenarios);
    println!("{markdown}");
    if let Ok(dir) = std::env::var("TUNNEL_LATTICE_BENCH_OUT") {
        let dir = std::path::Path::new(&dir);
        let stem = format!("device-{}-{}", meta.os, meta.build);
        let written = std::fs::create_dir_all(dir)
            .and_then(|()| {
                std::fs::write(
                    dir.join(format!("{stem}.json")),
                    report::to_json(&meta, &scenarios),
                )
            })
            .and_then(|()| std::fs::write(dir.join(format!("{stem}.md")), &markdown));
        if let Err(err) = written {
            eprintln!("device bench: writing results to {}: {err}", dir.display());
            std::process::exit(1);
        }
    }
    if scenarios
        .iter()
        .any(|scenario| matches!(scenario.outcome, Outcome::Failed(_)))
    {
        eprintln!("device bench: at least one scenario failed");
        std::process::exit(1);
    }
}
