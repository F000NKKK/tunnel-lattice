//! The UDP socket side of the benchmark: readiness probe, round-trip
//! latency, windowed echo throughput, and the stop/wake datagrams. The
//! socket is bound to the device's address and connected to the peer, so
//! everything it sends is routed into the device and only echoes from the
//! peer reach it. The same code measures every packet path.

use std::io::ErrorKind;
use std::net::{SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::packet::STOP_MAGIC;
use crate::report::{self, Latency, Throughput};

/// Binds to `local` (retrying while the address is not usable yet, e.g.
/// during Windows duplicate-address detection) and connects to `peer`.
pub fn connect(local: SocketAddrV4, peer: SocketAddrV4) -> Result<UdpSocket, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let socket = loop {
        match UdpSocket::bind(local) {
            Ok(socket) => break socket,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                thread::sleep(Duration::from_millis(250));
            }
            Err(err) => return Err(format!("bind {local}: {err}")),
        }
    };
    socket
        .connect(peer)
        .map_err(|err| format!("connect {peer}: {err}"))?;
    Ok(socket)
}

/// A datagram of `payload` bytes whose first eight carry `seq`.
fn datagram(payload: usize, seq: u64) -> Vec<u8> {
    let mut buf = vec![0x5a; payload.max(8)];
    buf[..8].copy_from_slice(&seq.to_le_bytes());
    buf
}

fn seq_of(buf: &[u8]) -> Option<u64> {
    buf.get(..8)
        .map(|bytes| u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
}

fn is_timeout(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// Sends one datagram and waits up to `timeout` for its echo; `Ok(None)`
/// on a timeout. Stale echoes of earlier datagrams are skipped.
fn round_trip(
    socket: &UdpSocket,
    out: &[u8],
    timeout: Duration,
) -> Result<Option<Duration>, String> {
    let seq = seq_of(out);
    let mut buf = vec![0u8; out.len() + 64];
    let start = Instant::now();
    socket.send(out).map_err(|err| format!("send: {err}"))?;
    loop {
        let left = timeout.saturating_sub(start.elapsed());
        if left.is_zero() {
            return Ok(None);
        }
        socket
            .set_read_timeout(Some(left))
            .map_err(|err| format!("set_read_timeout: {err}"))?;
        match socket.recv(&mut buf) {
            Ok(n) if n == out.len() && seq_of(&buf[..n]) == seq => {
                return Ok(Some(start.elapsed()));
            }
            Ok(_) => {}
            Err(err) if is_timeout(&err) => return Ok(None),
            // A refused or unreachable echo (e.g. an ICMP error before the
            // route is ready) is a lost datagram, not a failure.
            Err(err) if err.kind() == ErrorKind::ConnectionRefused => return Ok(None),
            Err(err) => return Err(format!("recv: {err}")),
        }
    }
}

/// Sends probes until one comes back, for up to `timeout`.
pub fn wait_ready(socket: &UdpSocket, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut seq = u64::MAX / 2;
    while Instant::now() < deadline {
        seq += 1;
        // Sending can fail while the address or route is still settling.
        if let Ok(Some(_)) = round_trip(socket, &datagram(32, seq), Duration::from_millis(250)) {
            return Ok(());
        }
    }
    Err(format!("no echo within {} s", timeout.as_secs()))
}

/// `warmup` then `count` sequential round trips of `payload`-byte
/// datagrams, one in flight at a time.
pub fn latency(
    socket: &UdpSocket,
    payload: usize,
    warmup: usize,
    count: usize,
) -> Result<Latency, String> {
    let mut rtts = Vec::with_capacity(count);
    let mut lost = 0u64;
    for seq in 0..(warmup + count) as u64 {
        let rtt = round_trip(socket, &datagram(payload, seq), Duration::from_millis(500))?;
        if seq < warmup as u64 {
            continue;
        }
        match rtt {
            Some(rtt) => rtts.push(u64::try_from(rtt.as_nanos()).unwrap_or(u64::MAX)),
            None => lost += 1,
        }
        if lost > 20 && lost as usize > rtts.len() {
            return Err(format!(
                "{lost} of {} datagrams lost",
                lost as usize + rtts.len()
            ));
        }
    }
    report::latency(rtts, lost).ok_or_else(|| "no round trip completed".to_owned())
}

/// Echo throughput: one thread sends while fewer than `window` datagrams
/// are in flight, another receives the echoes. After a 0.5 s warm-up the
/// echoes received in `secs` are counted. A window stuck for 100 ms
/// without an echo is written off as lost, so drops cannot stall it.
pub fn throughput(
    socket: &UdpSocket,
    payload: usize,
    wire_len: usize,
    secs: f64,
    window: u64,
) -> Result<Throughput, String> {
    let sender_socket = socket
        .try_clone()
        .map_err(|err| format!("clone socket: {err}"))?;
    let receiver_socket = socket
        .try_clone()
        .map_err(|err| format!("clone socket: {err}"))?;
    receiver_socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|err| format!("set_read_timeout: {err}"))?;

    let sent = Arc::new(AtomicU64::new(0));
    let received = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let sender = {
        let (sent, received, stop) = (Arc::clone(&sent), Arc::clone(&received), Arc::clone(&stop));
        thread::spawn(move || -> Result<(), String> {
            let out = datagram(payload, 0);
            let mut count = 0u64;
            let mut written_off = 0u64;
            let mut last_received = 0u64;
            let mut last_progress = Instant::now();
            let mut errors = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let echoed = received.load(Ordering::Relaxed);
                if echoed != last_received {
                    last_received = echoed;
                    last_progress = Instant::now();
                }
                let in_flight = count.saturating_sub(echoed).saturating_sub(written_off);
                if in_flight >= window {
                    if last_progress.elapsed() > Duration::from_millis(100) {
                        written_off += in_flight;
                        last_progress = Instant::now();
                    } else {
                        thread::park_timeout(Duration::from_millis(1));
                    }
                    continue;
                }
                match sender_socket.send(&out) {
                    Ok(_) => {
                        count += 1;
                        sent.store(count, Ordering::Relaxed);
                    }
                    // Transient: a full socket buffer, or an ICMP error
                    // reported on the connected socket.
                    Err(_) => {
                        errors += 1;
                        if errors > 1_000_000 {
                            return Err("send keeps failing".to_owned());
                        }
                        thread::yield_now();
                    }
                }
            }
            Ok(())
        })
    };
    let receiver = {
        let (received, stop) = (Arc::clone(&received), Arc::clone(&stop));
        let sender_thread = sender.thread().clone();
        thread::spawn(move || {
            let mut buf = vec![0u8; payload + 64];
            let mut count = 0u64;
            loop {
                match receiver_socket.recv(&mut buf) {
                    Ok(n) if n == payload.max(8) => {
                        count += 1;
                        received.store(count, Ordering::Relaxed);
                        sender_thread.unpark();
                    }
                    Ok(_) => {}
                    Err(err) if is_timeout(&err) && stop.load(Ordering::Relaxed) => break,
                    Err(_) => {}
                }
            }
        })
    };

    thread::sleep(Duration::from_millis(500));
    let (start, start_count) = (Instant::now(), received.load(Ordering::Relaxed));
    thread::sleep(Duration::from_secs_f64(secs));
    let (elapsed, end_count) = (start.elapsed(), received.load(Ordering::Relaxed));
    stop.store(true, Ordering::Relaxed);
    let sent_result = sender
        .join()
        .map_err(|_| "the sender thread panicked".to_owned())?;
    receiver
        .join()
        .map_err(|_| "the receiver thread panicked".to_owned())?;
    sent_result?;

    let round_trips = end_count - start_count;
    if round_trips == 0 {
        return Err("no echo during the throughput phase".to_owned());
    }
    Ok(report::throughput(
        elapsed.as_secs_f64(),
        round_trips,
        sent.load(Ordering::Relaxed),
        received.load(Ordering::Relaxed),
        wire_len,
    ))
}

/// Asks the reflector to stop.
pub fn send_stop(socket: &UdpSocket) {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(STOP_MAGIC);
    let _ = socket.send(&out);
}

/// Sends a datagram that is reflected like any other, to wake a receive
/// still waiting on the device after the reflector stopped.
pub fn send_wake(socket: &UdpSocket) {
    let _ = socket.send(&datagram(16, u64::MAX));
}
