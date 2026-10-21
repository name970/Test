#!/usr/bin/env bash
# Plauthium Public Testnet v1 launcher.
#
#   scripts/testnet.sh              interactive menu
#   scripts/testnet.sh --miner      join the public testnet and mine
#   scripts/testnet.sh --verifier   join the public testnet without mining
#   scripts/testnet.sh --local      run a local dev net of three nodes
#   scripts/testnet.sh --no-build   skip `cargo build` (with any of the above)
#
# SEED_NODE_IP is the IP address of a public seed node, dialed on port 30333. Leave it
# empty to start a new network as its first seed node.
#
# Public node: P2P on port 30333, explorer on http://127.0.0.1:8080/. Its miner key and
# chain data live in ${PLAUTHIUM_HOME:-${XDG_DATA_HOME:-~/.local/share}/plauthium}.
# Optional: PLAUTHIUM_MINING_THREADS (default 1), PLAUTHIUM_RESET=1 (wipe the chain
# data, keep the key).
#
# Local dev net: three nodes with a fresh genesis on P2P ports 30341-30343 and
# explorers on http://127.0.0.1:8081/ to :8083/, data in target/testnet/n1..n3.
SEED_NODE_IP="${SEED_NODE_IP:-}"

set -euo pipefail
SELF=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
cd "$(dirname "$0")/.."

P2P_PORT=30333
RPC_PORT=8080
BIN="${CARGO_TARGET_DIR:-./target}/release/plauthium-node"
TESTNET_DIR="./target/testnet"
DATA_HOME=${PLAUTHIUM_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/plauthium}

usage() {
  sed -n '2,8p' "$SELF" | sed 's/^# \{0,1\}//'
}

MODE=
BUILD=1
while [ $# -gt 0 ]; do
  case $1 in
    --miner) MODE=miner ;;
    --verifier) MODE=verifier ;;
    --local) MODE=local ;;
    --no-build) BUILD=0 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1 (try --help)" >&2; exit 2 ;;
  esac
  shift
done

warning() {
  local on= off=
  if [ -t 1 ]; then
    on=$'\033[1;31m'
    off=$'\033[0m'
  fi
  printf '%s' "$on"
  printf '%s\n' \
    "==============================================================================" \
    " WARNING: This is an experimental Testnet. Plauthium (PLAU) tokens have NO" \
    " real-world value. The creator makes no promises, guarantees, or financial" \
    " offerings. Do NOT share your private keys (\`miner_key.bin\`) or passwords" \
    " with anyone. Use at your own risk." \
    "=============================================================================="
  printf '%s' "$off"
}

choose_mode() {
  local answer
  if [ -t 0 ]; then
    exec 3<&0
  elif { exec 3</dev/tty; } 2>/dev/null; then
    :
  else
    echo "error: no terminal to ask on; pass --miner, --verifier or --local" >&2
    exit 2
  fi
  echo
  echo "Plauthium Public Testnet v1"
  echo "  1) Join Public Testnet as Miner"
  echo "  2) Join Public Testnet as Verifier Only (--no-mine)"
  echo "  3) Run Local Dev Net (Spawns 3 local nodes)"
  while :; do
    printf 'Choose 1, 2 or 3: '
    if ! IFS= read -r answer <&3; then
      echo
      echo "error: no choice made" >&2
      exit 2
    fi
    case $answer in
      1) MODE=miner ;;
      2) MODE=verifier ;;
      3) MODE=local ;;
      *) echo "Please answer 1, 2 or 3."; continue ;;
    esac
    break
  done
  exec 3<&-
}

if [ "${PLAUTHIUM_WARNED:-0}" != 1 ]; then
  warning
fi
[ -n "$MODE" ] || choose_mode

if [ "$BUILD" = 1 ]; then
  echo "Building plauthium-node (release)..."
  cargo build --release -p node
fi
[ -x "$BIN" ] || { echo "error: $BIN not found; run without --no-build" >&2; exit 1; }

require_free_port() {
  if (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; then
    echo "error: port $1 is already in use (by a running node or another program); stop it first" >&2
    exit 1
  fi
}

show_identity() {
  local out key_args=()
  if [ -n "${2:-}" ]; then
    key_args=(--key-file "$2")
  fi
  out=$("$BIN" key --data-dir "$1" ${key_args[@]+"${key_args[@]}"})
  printf '%s\n' "$out" | sed -n 's/^key_file: /  Miner key: /p; s/^owner_id: /  Owner id:  /p'
}

PIDS=()
stop_nodes() {
  local pid alive _
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do
    kill -INT "$pid" 2>/dev/null || true
  done
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    alive=0
    for pid in ${PIDS[@]+"${PIDS[@]}"}; do
      if kill -0 "$pid" 2>/dev/null; then alive=1; fi
    done
    [ "$alive" = 1 ] || break
    sleep 0.5
  done
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do
    kill -TERM "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
cleanup() {
  trap - INT TERM EXIT
  if [ "${#PIDS[@]}" -gt 0 ]; then
    echo || true
    echo "Stopping the nodes..." || true
    stop_nodes
  fi
  exit "$1"
}
trap 'cleanup 130' INT
trap 'cleanup 143' TERM
trap 'cleanup $?' EXIT

NODE_EXIT=1
nodes_alive() {
  local n=0 pid status
  for pid in "${PIDS[@]}"; do
    n=$((n + 1))
    if ! kill -0 "$pid" 2>/dev/null; then
      status=0
      wait "$pid" 2>/dev/null || status=$?
      if [ "$status" = 0 ]; then
        echo "Node $n stopped."
        NODE_EXIT=0
      else
        echo "error: node $n exited (status $status); see its log lines above" >&2
      fi
      return 1
    fi
  done
}

run_public() {
  local data_dir=$DATA_HOME/testnet-v1 key_file=$DATA_HOME/miner_key.bin args=()
  if [ -n "$SEED_NODE_IP" ]; then
    case $SEED_NODE_IP in
      *:*) args+=(--peer "[$SEED_NODE_IP]:$P2P_PORT") ;;
      *) args+=(--peer "$SEED_NODE_IP:$P2P_PORT") ;;
    esac
  fi
  if [ "$MODE" = miner ]; then
    args+=(--mining-threads "${PLAUTHIUM_MINING_THREADS:-1}")
    if [ -n "$SEED_NODE_IP" ]; then
      args+=(--mine-when-synced)
    fi
  else
    args+=(--no-mine)
  fi
  if [ "${PLAUTHIUM_RESET:-0}" = 1 ]; then
    args+=(--reset)
  fi
  require_free_port "$P2P_PORT"
  require_free_port "$RPC_PORT"

  mkdir -p "$data_dir"
  echo
  if [ "$MODE" = miner ] && [ -n "$SEED_NODE_IP" ]; then
    echo "Joining Plauthium Public Testnet v1 as a miner (mining once caught up)"
  elif [ "$MODE" = miner ]; then
    echo "Starting Plauthium Public Testnet v1 as its first seed node (mining)"
  else
    echo "Joining Plauthium Public Testnet v1 as a verifier"
  fi
  show_identity "$data_dir" "$key_file"
  echo "  Seed node: ${SEED_NODE_IP:-none}"
  echo "  P2P:       port $P2P_PORT"
  echo "  Explorer:  http://127.0.0.1:$RPC_PORT/"
  echo "Press Ctrl-C to stop."
  "$BIN" run \
    --data-dir "$data_dir" \
    --key-file "$key_file" \
    --p2p-listen "0.0.0.0:$P2P_PORT" \
    --rpc-listen "127.0.0.1:$RPC_PORT" \
    ${args[@]+"${args[@]}"} &
  PIDS+=($!)
}

run_local() {
  local genesis_file=$TESTNET_DIR/genesis.json i j peers
  for i in 1 2 3; do
    require_free_port "3034$i"
    require_free_port "808$i"
  done
  mkdir -p "$TESTNET_DIR"
  "$BIN" genesis \
    --out "$genesis_file" \
    --seal-attempts 128 \
    --target-block-secs 5 \
    --blocks-per-epoch 12
  echo
  echo "Plauthium local dev net (3 nodes, fresh genesis)"
  for i in 1 2 3; do
    echo "Node $i: explorer http://127.0.0.1:808$i/"
    show_identity "$TESTNET_DIR/n$i"
  done
  echo "Jobs become fundable at height 24 (2 x blocks-per-epoch)."
  echo "Press Ctrl-C to stop all nodes."
  for i in 1 2 3; do
    peers=()
    for j in 1 2 3; do
      if [ "$j" != "$i" ]; then
        peers+=(--peer "127.0.0.1:3034$j")
      fi
    done
    "$BIN" run \
      --data-dir "$TESTNET_DIR/n$i" \
      --genesis "$genesis_file" \
      --p2p-listen "127.0.0.1:3034$i" \
      --rpc-listen "127.0.0.1:808$i" \
      "${peers[@]}" \
      --mining-threads 1 \
      --reset &
    PIDS+=($!)
  done
}

case $MODE in
  miner|verifier) run_public ;;
  local) run_local ;;
esac

sleep 2
nodes_alive || exit "$NODE_EXIT"
while nodes_alive; do
  sleep 1
done
exit "$NODE_EXIT"
