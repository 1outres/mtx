#!/usr/bin/env bash
# Benchmark tmux vs mtx on macOS (single client, single pane).
# Metrics: startup time, attach time, stdout throughput, stdin RTT latency, resource usage.
set -euo pipefail

MTX_BUILD_TARGET=${MTX_BUILD_TARGET:-release}
RUNTIME_DIR=${RUNTIME_DIR:-/tmp/mxr-bench}
SOCK="${RUNTIME_DIR}/mxr.sock"
MTX_BIN_DIR=${MTX_BIN_DIR:-target/${MTX_BUILD_TARGET}}
MTXD="${MTX_BIN_DIR}/mxrd"
MTX="${MTX_BIN_DIR}/mxr"
TMUX_SESSION=mtxbench
HYPERFINE=$(command -v hyperfine || true)
BENCH_IDLE_MS=${BENCH_IDLE_MS:-200}
export MXR_BENCH_IDLE_MS=$BENCH_IDLE_MS

ensure_tools() {
  for c in tmux python3 /usr/bin/time; do
    command -v "$c" >/dev/null || {
      echo "missing dependency: $c" >&2
      exit 1
    }
  done
}

build_mtx() {
  echo "==> cargo build --${MTX_BUILD_TARGET} -p mxrd -p mxr"
  cargo build --"${MTX_BUILD_TARGET}" -p mxrd -p mxr >/dev/null
}

prepare_runtime() {
  mkdir -p "$RUNTIME_DIR"
  rm -f "$SOCK"
}

wait_for_socket() {
  local sock=$1
  for _ in {1..200}; do
    [ -S "$sock" ] && return 0
    sleep 0.005
  done
  echo "socket $sock not ready" >&2
  return 1
}

start_mxrd() {
  prepare_runtime
  XDG_RUNTIME_DIR="$RUNTIME_DIR" "$MTXD" >/tmp/mxrd-bench.log 2>&1 &
  MXRD_PID=$!
  wait_for_socket "$SOCK"
}

stop_mxrd() {
  if [ -n "${MXRD_PID:-}" ]; then
    kill "$MXRD_PID" >/dev/null 2>&1 || true
    wait "$MXRD_PID" 2>/dev/null || true
    unset MXRD_PID
  fi
}

start_tmux() {
  tmux -L "$TMUX_SESSION" -f /dev/null kill-server >/dev/null 2>&1 || true
  tmux -L "$TMUX_SESSION" -f /dev/null new-session -d -s "$TMUX_SESSION"
}

stop_tmux() {
  tmux -L "$TMUX_SESSION" -f /dev/null kill-server >/dev/null 2>&1 || true
}

bench_startup_tmux() {
  echo "==> Startup time: tmux"
  if [ -n "$HYPERFINE" ]; then
    $HYPERFINE --warmup 2 "tmux -L $TMUX_SESSION -f /dev/null new-session -d -s $TMUX_SESSION ; tmux -L $TMUX_SESSION kill-server"
  else
    /usr/bin/time -lp sh -c "tmux -L $TMUX_SESSION -f /dev/null new-session -d -s $TMUX_SESSION ; tmux -L $TMUX_SESSION kill-server"
  fi
}

bench_startup_mtx() {
  echo "==> Startup time: mtx (mxrd only)"
  local cmd='bash -c '\''mkdir -p '"${RUNTIME_DIR}"'; rm -f '"${SOCK}"'; XDG_RUNTIME_DIR='"${RUNTIME_DIR}"' '"${MTXD}"' >/dev/null 2>&1 & pid=$!; for _ in $(seq 1 200); do [ -S '"${SOCK}"' ] && break; sleep 0.005; done; kill $pid; wait $pid 2>/dev/null || true'\'''
  if [ -n "$HYPERFINE" ]; then
    $HYPERFINE --warmup 2 "$cmd"
  else
    /usr/bin/time -lp bash -c "mkdir -p ${RUNTIME_DIR}; rm -f ${SOCK}; XDG_RUNTIME_DIR=${RUNTIME_DIR} ${MTXD} >/dev/null 2>&1 & pid=\$!; for _ in \$(seq 1 200); do [ -S ${SOCK} ] && break; sleep 0.005; done; kill \$pid; wait \$pid 2>/dev/null"
  fi
}

bench_attach_tmux() {
  echo "==> Attach time: tmux"
  start_tmux
  if [ -n "$HYPERFINE" ]; then
    $HYPERFINE --warmup 2 "printf 'exit\\n' | tmux -L $TMUX_SESSION -f /dev/null attach -t $TMUX_SESSION >/dev/null; true"
  else
    /usr/bin/time -lp sh -c "printf 'exit\\n' | tmux -L $TMUX_SESSION -f /dev/null attach -t $TMUX_SESSION >/dev/null; true"
  fi
  stop_tmux
}

bench_attach_mtx() {
  echo "==> Attach time: mtx"
  start_mxrd
  XDG_RUNTIME_DIR="$RUNTIME_DIR" "$MTX" create bench >/dev/null
  if [ -n "$HYPERFINE" ]; then
    XDG_RUNTIME_DIR="$RUNTIME_DIR" $HYPERFINE --warmup 2 "printf 'exit\\n' | $MTX attach 0 >/dev/null; true"
  else
    XDG_RUNTIME_DIR="$RUNTIME_DIR" /usr/bin/time -lp sh -c "printf 'exit\\n' | $MTX attach 0 >/dev/null; true"
  fi
  stop_mxrd
}

bench_throughput_tmux() {
  echo "==> Stdout throughput (50MB): tmux"
  start_tmux
  /usr/bin/time -lp sh -c "printf 'yes | head -c 50000000\nexit\n' | script -q /dev/null tmux -L $TMUX_SESSION -f /dev/null attach -t $TMUX_SESSION > /dev/null" || true
  stop_tmux
}

bench_throughput_mtx() {
  echo "==> Stdout throughput (50MB): mtx"
  start_mxrd
  XDG_RUNTIME_DIR="$RUNTIME_DIR" "$MTX" create bench >/dev/null
  XDG_RUNTIME_DIR="$RUNTIME_DIR" /usr/bin/time -lp sh -c "printf 'yes | head -c 50000000\nexit\n' | script -q /dev/null $MTX attach 0 > /dev/null" || true
  stop_mxrd
}

bench_latency_tmux() {
  echo "==> stdin RTT latency (cat, raw): tmux"
  start_tmux
  python3 "$(dirname "$0")/rtt_latency.py" --cmd "tmux -L $TMUX_SESSION -f /dev/null attach -t $TMUX_SESSION" --setup "stty raw -echo; cat" --iters 500
  stop_tmux
}

bench_latency_mtx() {
  echo "==> stdin RTT latency (cat, raw): mtx"
  start_mxrd
  XDG_RUNTIME_DIR="$RUNTIME_DIR" "$MTX" create bench >/dev/null
  XDG_RUNTIME_DIR="$RUNTIME_DIR" python3 "$(dirname "$0")/rtt_latency.py" --cmd "$MTX attach 0" --setup "stty raw -echo; cat" --iters 500
  stop_mxrd
}

main() {
  ensure_tools
  build_mtx
  bench_startup_tmux
  bench_startup_mtx
  bench_attach_tmux
  bench_attach_mtx
  bench_throughput_tmux
  bench_throughput_mtx
  bench_latency_tmux
  bench_latency_mtx
}

main "$@"
