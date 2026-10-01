#!/usr/bin/env bash
# iperf3 forwarder benchmark: raw tun-rs baselines and tunnel-lattice
# forwarding between two TUN devices, measured the same way in the same run.
#
# Usage:
#   scripts/bench-forward.sh build [--rustflags FLAGS]
#       Unprivileged. Builds the forwarders of bench/forwarder once per build
#       set (sync, tokio, async-io) into target/bench-forward/<set>/ and
#       records the git state, toolchain and resolved crate versions in
#       target/bench-forward/meta.json. FLAGS default to
#       "-C target-cpu=native".
#
#   sudo scripts/bench-forward.sh run [--reps N] [--duration S]
#                                     [--variants id,id|all] [--out DIR]
#                                     [--fail-fast]
#       Root (TUN devices, a network namespace, routes). Runs only the
#       prebuilt binaries; never runs cargo. Default 5 repetitions of 10 s
#       per variant, one warm-up run first, variant order rotated every
#       repetition. Results go to DIR (default
#       target/bench-forward/runs/<UTC time>).
#
#   scripts/bench-forward.sh report DIR [--allow-incomplete]
#       Unprivileged. Writes DIR/results.json and DIR/results.md and prints
#       the Markdown table. Refuses a variant with failed runs unless
#       --allow-incomplete is given.
#
# Topology (per run): the forwarder opens IFACE1 and IFACE2; IFACE2 moves
# into the network namespace NS; iperf3 -c runs on the host against an
# iperf3 server in NS, so every packet crosses the forwarder both ways.
# Override the names with TL_BENCH_IFACE1, TL_BENCH_IFACE2, TL_BENCH_IP1,
# TL_BENCH_IP2, TL_BENCH_NS (defaults tun11, tun22, 10.0.1.1, 10.0.2.1,
# ns1). `run` refuses to start if any of them already exists, and removes
# everything it created on success, failure, Ctrl-C and SIGTERM.

set -Eeuo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
MANIFEST="$ROOT/bench/forwarder/Cargo.toml"
REGISTRY="$ROOT/bench/forwarder/variants.tsv"
BUILD_ROOT="$ROOT/target/bench-forward"
BUILD_SETS=(sync tokio async-io)
REPORT_BIN="$BUILD_ROOT/sync/release/forwarder-report"

IF1=${TL_BENCH_IFACE1:-tun11}
IF2=${TL_BENCH_IFACE2:-tun22}
IP1=${TL_BENCH_IP1:-10.0.1.1}
IP2=${TL_BENCH_IP2:-10.0.2.1}
NS=${TL_BENCH_NS:-ns1}
MTU=1500
IPERF3_PORT=5201

# State owned by the current run; `cleanup` acts only on what is set here.
FWD_PID=""
SRV_PID=""
SAMPLER_PID=""
ROUTE_ADDED=0
NS_CREATED=0
CHECK_IFACES=0
OUT=""

log() {
    printf '[bench-forward] %s\n' "$*" >&2
}

die() {
    log "error: $*"
    exit 1
}

usage() {
    sed -n '2,/^$/s/^# \{0,1\}//p' "${BASH_SOURCE[0]}" >&2
    exit 64
}

require_report_bin() {
    [[ -x $REPORT_BIN ]] || die "$REPORT_BIN not found; run 'scripts/bench-forward.sh build' first"
}

# ---------------------------------------------------------------- build ---

cmd_build() {
    local rustflags="-C target-cpu=native"
    while (($#)); do
        case $1 in
            --rustflags)
                (($# >= 2)) || usage
                rustflags=$2
                shift 2
                ;;
            *) usage ;;
        esac
    done

    local set
    for set in "${BUILD_SETS[@]}"; do
        log "building the $set forwarders"
        RUSTFLAGS="$rustflags" cargo build --release \
            --manifest-path "$MANIFEST" \
            --no-default-features --features "$set" \
            --target-dir "$BUILD_ROOT/$set"
    done

    # All build sets share bench/forwarder/Cargo.lock, so they resolve the
    # same tun-rs; the `sync` set is enough to name both versions.
    cargo metadata --format-version 1 --manifest-path "$MANIFEST" \
        --no-default-features --features sync >"$BUILD_ROOT/cargo-metadata.json"
    local sha dirty=false
    sha=$(git -C "$ROOT" rev-parse HEAD)
    if [[ -n $(git -C "$ROOT" status --porcelain) ]]; then
        dirty=true
    fi
    "$REPORT_BIN" meta --out "$BUILD_ROOT/meta.json" \
        --cargo-metadata "$BUILD_ROOT/cargo-metadata.json" \
        "rustc=$(rustc -V)" \
        "rustflags=$rustflags" \
        "git_sha=$sha" \
        "git_dirty:=$dirty"
    log "built; wrote $BUILD_ROOT/meta.json"
}

# --------------------------------------------------------------- report ---

cmd_report() {
    (($# >= 1)) || usage
    local dir=$1 allow=()
    shift
    while (($#)); do
        case $1 in
            --allow-incomplete)
                allow=(--allow-incomplete)
                shift
                ;;
            *) usage ;;
        esac
    done
    require_report_bin
    "$REPORT_BIN" collect "$dir"
    local table
    table=$("$REPORT_BIN" markdown "$dir/results.json" "${allow[@]}")
    printf '%s\n' "$table" >"$dir/results.md"
    printf '%s\n' "$table"
}

# ------------------------------------------------------------------ run ---

# Removes everything the current run created. Idempotent; never fails.
cleanup() {
    local errexit=0 i
    [[ $- == *e* ]] && errexit=1
    set +e

    if [[ -n $SAMPLER_PID ]]; then
        kill "$SAMPLER_PID" 2>/dev/null
        wait "$SAMPLER_PID" 2>/dev/null
        SAMPLER_PID=""
    fi
    if [[ -n $SRV_PID ]]; then
        kill "$SRV_PID" 2>/dev/null
        wait "$SRV_PID" 2>/dev/null
        SRV_PID=""
    fi
    if [[ -n $FWD_PID ]]; then
        # Closing the forwarder's file descriptors removes its
        # (non-persistent) TUN devices, also the one moved into $NS.
        kill -TERM "$FWD_PID" 2>/dev/null
        for ((i = 0; i < 50; i++)); do
            kill -0 "$FWD_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$FWD_PID" 2>/dev/null
        wait "$FWD_PID" 2>/dev/null
        FWD_PID=""
    fi
    if ((ROUTE_ADDED)); then
        ip route del "$IP2/32" 2>/dev/null
        ROUTE_ADDED=0
    fi
    if ((NS_CREATED)); then
        ip netns del "$NS" 2>/dev/null || log "warning: could not delete netns $NS"
        NS_CREATED=0
    fi
    if ((CHECK_IFACES)); then
        local iface
        for iface in "$IF1" "$IF2"; do
            for ((i = 0; i < 50; i++)); do
                ip link show "$iface" >/dev/null 2>&1 || break
                sleep 0.1
            done
            if ip link show "$iface" >/dev/null 2>&1; then
                log "warning: $iface still exists after the forwarder exited; deleting it"
                ip link del "$iface"
            fi
        done
        CHECK_IFACES=0
    fi

    if ((errexit)); then
        set -e
    fi
    return 0
}

on_exit() {
    local status=$?
    cleanup
    # Hand the results back to the user who ran `sudo`.
    if [[ -n $OUT && -n ${SUDO_UID:-} && -d $OUT ]]; then
        chown -R "$SUDO_UID:${SUDO_GID:-$SUDO_UID}" "$OUT" 2>/dev/null || true
    fi
    exit "$status"
}

# Appends one line per second until the process exits or this is killed:
#   t_ns  utime_ticks  stime_ticks  vmrss_kb  ps_pcpu
# utime/stime are the process totals over all threads (/proc/PID/stat
# fields 14 and 15, counted after the last ')' since the command name may
# contain spaces or parentheses). t_ns is wall-clock time
# ($EPOCHREALTIME); only differences between samples are used.
sampler() {
    local pid=$1 file=$2 now stat rest rss pcpu
    local -a fields
    set +e
    trap 'exit 0' TERM
    while kill -0 "$pid" 2>/dev/null; do
        now=${EPOCHREALTIME//[.,]/}
        stat=$(<"/proc/$pid/stat") || break
        rest=${stat##*) }
        read -r -a fields <<<"$rest"
        rss=$(awk '/^VmRSS:/ { print $2 }' "/proc/$pid/status") || break
        pcpu=$(ps -o %cpu= -p "$pid") || break
        printf '%s000\t%s\t%s\t%s\t%s\n' \
            "$now" "${fields[11]}" "${fields[12]}" "$rss" "${pcpu// /}" >>"$file"
        sleep 1
    done
}

# Runs one variant once. Records the outcome in $dir/run.json and returns 0
# if it succeeded, 1 otherwise. Never aborts the script on a failed step.
run_one() {
    local id=$1 rep=$2 order=$3
    local dir="$OUT/runs/$id/$rep"
    local bin="$BUILD_ROOT/${SET[$id]}/release/${BIN[$id]}"
    local -a extra=()
    if [[ ${ARGS[$id]} != - ]]; then
        read -r -a extra <<<"${ARGS[$id]}"
    fi
    mkdir -p "$dir"
    log "run $order: $id, repetition $rep"

    local error=""
    run_steps "$dir" "$bin" "${extra[@]}" || true
    cleanup

    local ok=true error_field="error:=null"
    if [[ -n $error ]]; then
        ok=false
        error_field="error=$error"
        log "run $order failed: $error"
    fi
    "$REPORT_BIN" kv --out "$dir/run.json" \
        "variant=$id" "rep=$rep" "order:=$order" "ok:=$ok" "$error_field"
    sleep 1
    [[ $ok == true ]]
}

# The steps of one run. Sets `error` (in run_one's scope) on the first
# failure and returns 1. Every step is checked explicitly: `set -e` does not
# apply inside a function called from a `||` list.
run_steps() {
    local dir=$1 bin=$2
    shift 2
    local i

    CHECK_IFACES=1
    "$bin" --iface1 "$IF1" --ip1 "$IP1" --iface2 "$IF2" --ip2 "$IP2" --mtu "$MTU" "$@" \
        >"$dir/forwarder.log" 2>&1 &
    FWD_PID=$!

    # Ready: the forwarder printed its ready line and both devices exist.
    for ((i = 0; i < 100; i++)); do
        if ! kill -0 "$FWD_PID" 2>/dev/null; then
            error="forwarder exited before it was ready: $(tail -n 3 "$dir/forwarder.log" | tr '\n' ' ')"
            return 1
        fi
        if grep -q '^ready ' "$dir/forwarder.log" &&
            ip link show "$IF1" >/dev/null 2>&1 && ip link show "$IF2" >/dev/null 2>&1; then
            break
        fi
        sleep 0.1
    done
    if ((i == 100)); then
        error="forwarder not ready within 10 s"
        return 1
    fi

    # The same addresses, routes and namespace for every variant. The tun-rs
    # baselines already assigned their addresses; `replace` makes that a
    # no-op, and moving IFACE2 into the namespace drops its address anyway.
    if ! { ip addr replace "$IP1/24" dev "$IF1" && ip link set "$IF1" up; }; then
        error="host-side setup of $IF1 failed"
        return 1
    fi
    ip netns add "$NS" || {
        error="ip netns add $NS failed"
        return 1
    }
    NS_CREATED=1
    if ! {
        ip link set "$IF2" netns "$NS" &&
            ip -n "$NS" addr replace "$IP2/24" dev "$IF2" &&
            ip -n "$NS" link set "$IF2" up &&
            ip -n "$NS" route replace "$IP1/32" dev "$IF2"
    }; then
        error="namespace setup of $IF2 failed"
        return 1
    fi
    ip route replace "$IP2/32" dev "$IF1" || {
        error="host route to $IP2 failed"
        return 1
    }
    ROUTE_ADDED=1

    ip netns exec "$NS" iperf3 -s -1 -B "$IP2" -p "$IPERF3_PORT" >"$dir/server.log" 2>&1 &
    SRV_PID=$!
    for ((i = 0; i < 50; i++)); do
        if [[ -n $(ip netns exec "$NS" ss -Hltn "sport = :$IPERF3_PORT" 2>/dev/null) ]]; then
            break
        fi
        if ! kill -0 "$SRV_PID" 2>/dev/null; then
            error="iperf3 server exited: $(tr '\n' ' ' <"$dir/server.log")"
            return 1
        fi
        sleep 0.1
    done
    if ((i == 50)); then
        error="iperf3 server not listening within 5 s"
        return 1
    fi

    sampler "$FWD_PID" "$dir/samples.tsv" &
    SAMPLER_PID=$!

    local rc=0
    # --foreground keeps iperf3 in this process group, so Ctrl-C reaches it
    # at once instead of waiting out the timeout.
    timeout --foreground "$((DURATION + 20))" \
        iperf3 -c "$IP2" -p "$IPERF3_PORT" -t "$DURATION" -J --connect-timeout 3000 \
        >"$dir/iperf3.json" 2>"$dir/client.stderr" || rc=$?

    kill "$SAMPLER_PID" 2>/dev/null
    wait "$SAMPLER_PID" 2>/dev/null
    SAMPLER_PID=""

    if ! kill -0 "$FWD_PID" 2>/dev/null; then
        error="forwarder died during the run: $(tail -n 3 "$dir/forwarder.log" | tr '\n' ' ')"
        return 1
    fi
    local hwm
    hwm=$(awk '/^VmHWM:/ { print $2 }' "/proc/$FWD_PID/status" 2>/dev/null) || hwm=""
    if [[ -n $hwm ]]; then
        printf '#hwm_kb\t%s\n' "$hwm" >>"$dir/samples.tsv"
    fi

    if ((rc != 0)); then
        error="iperf3 client exited with $rc"
    fi
    local check
    if ! check=$("$REPORT_BIN" iperf3-check "$dir/iperf3.json" 2>&1); then
        error="${error:+$error; }$check"
    fi
    [[ -z $error ]]
}

cmd_run() {
    local reps=5 variants=all fail_fast=0
    DURATION=10
    while (($#)); do
        case $1 in
            --reps | --duration | --variants | --out)
                (($# >= 2)) || usage
                case $1 in
                    --reps) reps=$2 ;;
                    --duration) DURATION=$2 ;;
                    --variants) variants=$2 ;;
                    --out) OUT=$2 ;;
                esac
                shift 2
                ;;
            --fail-fast)
                fail_fast=1
                shift
                ;;
            *) usage ;;
        esac
    done
    [[ $reps =~ ^[1-9][0-9]*$ ]] || die "--reps must be a positive integer"
    [[ $DURATION =~ ^[1-9][0-9]*$ ]] || die "--duration must be a positive integer"

    ((EUID == 0)) || die "run needs root (TUN devices, a network namespace, routes)"
    local tool
    for tool in iperf3 ip ss getconf ps timeout awk; do
        command -v "$tool" >/dev/null || die "$tool not found"
    done
    [[ -c /dev/net/tun ]] || die "/dev/net/tun is missing"
    require_report_bin
    [[ -f $BUILD_ROOT/meta.json ]] || die "$BUILD_ROOT/meta.json not found; run build first"
    if [[ -e /run/netns/$NS ]]; then
        die "network namespace $NS already exists; refusing to touch it"
    fi
    local iface
    for iface in "$IF1" "$IF2"; do
        if ip link show "$iface" >/dev/null 2>&1; then
            die "interface $iface already exists; refusing to touch it"
        fi
    done

    # The registry; "-" marks an empty field.
    declare -gA SET=() BIN=() ARGS=() BASE=()
    local -a ids=() selected=()
    local id set bin args base _rest
    while IFS=$'\t' read -r id set bin args base _rest; do
        [[ -z $id || $id == \#* ]] && continue
        ids+=("$id")
        SET[$id]=$set
        BIN[$id]=$bin
        ARGS[$id]=$args
        BASE[$id]=$base
    done <"$REGISTRY"
    if [[ $variants == all ]]; then
        selected=("${ids[@]}")
    else
        IFS=, read -r -a selected <<<"$variants"
    fi
    ((${#selected[@]})) || die "no variants selected"
    for id in "${selected[@]}"; do
        [[ -n ${SET[$id]:-} ]] || die "unknown variant $id (see $REGISTRY)"
        [[ -x $BUILD_ROOT/${SET[$id]}/release/${BIN[$id]} ]] ||
            die "${BIN[$id]} not built for ${SET[$id]}; run build first"
    done

    if [[ -z $OUT ]]; then
        OUT="$BUILD_ROOT/runs/$(date -u +%Y%m%dT%H%M%SZ)"
    fi
    if [[ -e $OUT && -n $(ls -A "$OUT") ]]; then
        die "$OUT is not empty"
    fi
    mkdir -p "$OUT/runs"
    trap on_exit EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    cp "$REGISTRY" "$OUT/variants.tsv"
    cp "$BUILD_ROOT/meta.json" "$OUT/meta.json"
    local os cpu_model iperf3_version image=""
    # shellcheck source=/dev/null
    os=$(. /etc/os-release && printf '%s' "${PRETTY_NAME:-$NAME}") || os=unknown
    cpu_model=$(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo)
    iperf3_version=$(iperf3 --version 2>&1 | awk 'NR == 1 { print $2 }')
    if [[ -n ${ImageOS:-} ]]; then
        image="$ImageOS${ImageVersion:+ $ImageVersion}"
    fi
    local github_actions=false
    [[ ${GITHUB_ACTIONS:-} == true ]] && github_actions=true
    local selected_csv
    selected_csv=$(IFS=,; printf '%s' "${selected[*]}")
    "$REPORT_BIN" kv --out "$OUT/env.json" \
        "created_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        "kernel=$(uname -r)" \
        "cpu_model=$cpu_model" \
        "nproc:=$(nproc)" \
        "os=$os" \
        "github_actions:=$github_actions" \
        "runner_image=$image" \
        "iperf3=$iperf3_version" \
        "clk_tck:=$(getconf CLK_TCK)" \
        "duration_s:=$DURATION" \
        "reps:=$reps" \
        "warmup_runs:=1" \
        "variants=$selected_csv" \
        "iface1=$IF1" "ip1=$IP1" "iface2=$IF2" "ip2=$IP2" "netns=$NS" \
        "mtu:=$MTU"

    # Warm-up: the first selected baseline (or the first variant), not
    # counted in the results.
    local warmup=${selected[0]}
    for id in "${selected[@]}"; do
        if [[ ${BASE[$id]} == - ]]; then
            warmup=$id
            break
        fi
    done
    local order=0 failed=0 failed_baseline=0
    order=$((order + 1))
    run_one "$warmup" warmup "$order" || log "warm-up run failed (not counted)"

    local n=${#selected[@]} rep k
    for ((rep = 1; rep <= reps; rep++)); do
        for ((k = 0; k < n; k++)); do
            id=${selected[$(((k + rep) % n))]}
            order=$((order + 1))
            if ! run_one "$id" "$rep" "$order"; then
                failed=$((failed + 1))
                if [[ ${BASE[$id]} == - ]]; then
                    failed_baseline=$((failed_baseline + 1))
                fi
                if ((fail_fast)); then
                    die "stopping after the first failed run (--fail-fast)"
                fi
            fi
        done
    done

    log "done: $((order - 1)) measured runs, $failed failed; results in $OUT"
    if ((failed_baseline)); then
        die "$failed_baseline baseline run(s) failed; their paired ratios are missing"
    fi
}

(($# >= 1)) || usage
command=$1
shift
case $command in
    build) cmd_build "$@" ;;
    run) cmd_run "$@" ;;
    report) cmd_report "$@" ;;
    -h | --help | help) usage ;;
    *) usage ;;
esac
