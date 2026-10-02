#!/usr/bin/env bash
# Start, stop, or refresh the engine under the Qwen3.8-Flash-Next speed study.
#
# The engine is a long-lived external process, not part of the benchmark: a knob
# change costs a full model load, so measure.sh calls `ensure` and only pays that
# when the launch config in engine.sh actually changed.
#
#   serve.sh ensure   start it, or restart it if engine.sh changed
#   serve.sh stop     tear down what this script started (never other people's)
#   serve.sh status   report what is up and whether it matches engine.sh
#
# Never touches the developer's llamastash daemon. It does refuse to load a
# second engine while anything else holds the GPU, because a benchmark taken
# beside a resident model measures contention, not configuration.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
STATE="$ROOT/.auto/run"
STAMP="$STATE/config.id"
LOG="$STATE/engine.log"
PIDFILE="$STATE/engine.pid"
CONTAINER=""

# shellcheck disable=SC1091
. "$HERE/engine.sh"

oscar() { docker ps --format '{{.Names}}' | grep -qx "ar-halo-$PORT"; }

# Anything outside this script holding the GPU or several GiB of model RAM.
foreign_gpu_users() {
  local p
  for p in $(pgrep -x flash_serve; pgrep -x gufo-server; pgrep -f 'gufo serve';
            pgrep -f llama-server; pgrep -f vllm; pgrep -f sglang); do
    # Our own gufo child is recorded in the pidfile; the docker engine's parent
    # is dockerd, so flash_serve from a container we started is filtered by name.
    [ "$p" = "$(cat "$PIDFILE" 2>/dev/null || true)" ] && continue
    oscar && pgrep -x flash_serve >/dev/null 2>&1 && continue
    echo "$p $(ps -o comm= -p "$p" 2>/dev/null)"
  done | sort -u
}

stop_engine() {
  if [ -n "$CONTAINER" ] || oscar; then
    docker rm -f "ar-halo-$PORT" >/dev/null 2>&1 || true
  fi
  if [ -f "$PIDFILE" ]; then
    kill "$(cat "$PIDFILE")" 2>/dev/null || true
    rm -f "$PIDFILE"
  fi
  rm -f "$STAMP"
}

wait_ready() {
  local path=$1 budget=${2:-900} i=0
  while [ "$i" -lt "$budget" ]; do
    if curl -fsS -m 5 "http://127.0.0.1:$PORT$path" >/dev/null 2>&1; then
      return 0
    fi
    # A dead child never becomes ready; fail now instead of after the timeout.
    if [ -f "$PIDFILE" ] && ! kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
      echo "engine exited during startup; last log lines:" >&2
      tail -20 "$LOG" >&2
      return 1
    fi
    sleep 2; i=$((i + 2))
  done
  echo "engine not ready after ${budget}s; last log lines:" >&2
  tail -20 "$LOG" >&2
  return 1
}

start_engine() {
  mkdir -p "$STATE"
  : > "$LOG"
  if [ "$ENGINE" = gufo ]; then
    # shellcheck disable=SC2046
    nohup $(gufo_argv) >>"$LOG" 2>&1 &
    echo $! > "$PIDFILE"
    wait_ready /ready 1200
  else
    # shellcheck disable=SC2046
    CONTAINER="ar-halo-$PORT"
    docker rm -f "ar-halo-$PORT" >/dev/null 2>&1 || true
    # shellcheck disable=SC2046
    $(halo_argv) >>"$LOG" 2>&1
    wait_ready /health 900
  fi
}

case "${1:-ensure}" in
  stop)
    stop_engine
    echo "stopped"
    ;;
  status)
    want=$(engine_config_id)
    have=$(cat "$STAMP" 2>/dev/null || echo none)
    echo "engine=$ENGINE port=$PORT want=$want have=$have"
    ;;
  ensure)
    foreign=$(foreign_gpu_users)
    if [ -n "$foreign" ]; then
      echo "refusing to load a second engine; these hold the GPU:" >&2
      echo "$foreign" >&2
      echo "stop them first (for the daemon's model: llamastash stop --all)" >&2
      exit 3
    fi
    want=$(engine_config_id)
    if [ "$(cat "$STAMP" 2>/dev/null || echo none)" = "$want" ] &&
       curl -fsS -m 5 "http://127.0.0.1:${PORT}$([ "$ENGINE" = gufo ] && echo /ready || echo /health)" >/dev/null 2>&1; then
      echo "engine up on $PORT ($want)"
      exit 0
    fi
    stop_engine
    start_engine
    echo "$want" > "$STAMP"
    echo "engine up on $PORT ($want)"
    ;;
  *)
    echo "usage: serve.sh [ensure|stop|status]" >&2
    exit 2
    ;;
esac
