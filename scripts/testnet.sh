#!/usr/bin/env bash
# Plauthium Public Testnet v1 launcher.
#
#   scripts/testnet.sh              # asks which of the three to run
#   scripts/testnet.sh --join       # a) join the public testnet and mine
#   scripts/testnet.sh --verify     # b) join the public testnet, verification only (--no-mine)
#   scripts/testnet.sh --local      # c) local dev testnet: 3 mining nodes on this machine
#   scripts/testnet.sh --no-build   # skip `cargo build` (combine with any of the above)
#
# Public testnet (a, b): one node with the embedded genesis (node/genesis.testnet.json),
# dialing the bootnodes below. Its ML-DSA-87 miner key is PLAUTHIUM_HOME/miner_key.bin
# and its chain data PLAUTHIUM_HOME/testnet-v1 (PLAUTHIUM_HOME defaults to
# ${XDG_DATA_HOME:-~/.local/share}/plauthium, outside the workspace, so `cargo clean` or
# a fresh copy of the bundle keeps the identity; a node locks its data directory). Both are kept across runs, so a and b share one
# identity and one copy of the chain. In mode a the node mines only once it is connected
# and caught up. P2P listens on 0.0.0.0:30333 (forward that port to accept inbound
# peers); the explorer is http://127.0.0.1:8080/.
#
# Local dev testnet (c): node i (i = 1..3) peers on 127.0.0.1:3034i, serves its
# explorer on http://127.0.0.1:808i/ and keeps its data in target/testnet/ni. A fresh
# genesis is written on every run and the chain data is reset; the keys are kept.
#
# Every node creates its miner key on its first run and loads it afterwards; the key is
# shown before the nodes start. Ctrl-C stops the nodes gracefully.
#
# Environment overrides for the public testnet:
#   PLAUTHIUM_BOOTNODES       host:port list, space or comma separated. Empty: dial no one
#                             and mine alone (only for the first bootnode of a network)
#   PLAUTHIUM_HOME            key and chain data directory
#                             (default ${XDG_DATA_HOME:-~/.local/share}/plauthium)
#   PLAUTHIUM_P2P_LISTEN      P2P listen address (default 0.0.0.0:30333)
#   PLAUTHIUM_RPC_LISTEN      explorer and RPC address (default 127.0.0.1:8080)
#   PLAUTHIUM_MINING_THREADS  mining threads in mode a (default 1)
#   PLAUTHIUM_RESET=1         wipe the chain data (the key is kept) before starting
set -euo pipefail
SELF=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
cd "$(dirname "$0")/.."

DEFAULT_BOOTNODES="seed1.plauthium.network:30333 seed2.plauthium.network:30333"
BIN="${CARGO_TARGET_DIR:-./target}/release/plauthium-node"
TESTNET_DIR="./target/testnet"

usage() {
  sed -n '2,8p' "$SELF" | sed 's/^# \{0,1\}//'
}

MODE=
BUILD=1
while [ $# -gt 0 ]; do
  case $1 in
    --join) MODE=join ;;
    --verify) MODE=verify ;;
    --local) MODE=local ;;
    --no-build) BUILD=0 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1 (try --help)" >&2; exit 2 ;;
  esac
  shift
done

# The menu reads the answer from the terminal, even when stdin is a pipe (curl | bash).
choose_mode() {
  local answer
  if [ -t 0 ]; then
    exec 3<&0
  elif { exec 3</dev/tty; } 2>/dev/null; then
    :
  else
    echo "error: no terminal to ask on; pass --join, --verify or --local" >&2
    exit 2
  fi
  echo "============================================================"
  echo "Plauthium Public Testnet v1"
  echo "  a) Join the public testnet and mine"
  echo "  b) Join the public testnet, verification only (no mining)"
  echo "  c) Local dev testnet (3 mining nodes on this machine)"
  echo "============================================================"
  while :; do
    printf 'Choose a, b or c [a]: '
    if ! IFS= read -r answer <&3; then
      echo
      echo "error: no choice made" >&2
      exit 2
    fi
    case ${answer:-a} in
      a|A) MODE=join ;;
      b|B) MODE=verify ;;
      c|C) MODE=local ;;
      *) echo "Please answer a, b or c."; continue ;;
    esac
    break
  done
  exec 3<&-
}
[ -n "$MODE" ] || choose_mode

if [ "$BUILD" = 1 ]; then
  echo "Building plauthium-node (release)..."
  cargo build --release -p node
fi
[ -x "$BIN" ] || { echo "error: $BIN not found; run without --no-build" >&2; exit 1; }

# Refuses to start next to anything already listening on the port ($1); $2 says how to
# choose another port.
require_free_port() {
  if (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; then
    echo "error: port $1 is already in use (by a node from an earlier run, or another program)." >&2
    [ -z "${2:-}" ] || echo "       To use another port, set $2" >&2
    exit 1
  fi
}

# Creates the miner key if needed (data directory $1, optional key file $2) and prints
# the identity it gives the node.
show_key() {
  local out key_args=()
  if [ -n "${2:-}" ]; then
    key_args=(--key-file "$2")
  fi
  out=$("$BIN" key --data-dir "$1" ${key_args[@]+"${key_args[@]}"})
  printf '%s\n' "$out" | sed -n 's/^key_file: /  Miner key: /p; s/^status: /  Key:       /p; s/^owner_id: /  Owner id:  /p'
}

PIDS=()
stop_nodes() {
  local pid alive _
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do
    kill -INT "$pid" 2>/dev/null || true
  done
  # Give the nodes up to 10 s to flush their block logs, then terminate any that remain.
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

# A node exits with status 0 only after a graceful shutdown (Ctrl-C or kill), so that
# stops the launcher quietly; any other status is a failure.
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
  local home=${PLAUTHIUM_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/plauthium}
  local data_dir=$home/testnet-v1 key_file=$home/miner_key.bin
  local p2p=${PLAUTHIUM_P2P_LISTEN:-0.0.0.0:30333}
  local rpc=${PLAUTHIUM_RPC_LISTEN:-127.0.0.1:8080}
  local bootnodes=${PLAUTHIUM_BOOTNODES-$DEFAULT_BOOTNODES}
  local args=() b list=
  for b in ${bootnodes//,/ }; do
    args+=(--peer "$b")
    list="${list:+$list }$b"
  done
  bootnodes=$list
  if [ "$MODE" = join ]; then
    args+=(--mining-threads "${PLAUTHIUM_MINING_THREADS:-1}")
    # Mining before reaching the network would build a private fork from genesis.
    if [ -n "$bootnodes" ]; then
      args+=(--mine-when-synced)
    fi
  else
    args+=(--no-mine)
  fi
  if [ "${PLAUTHIUM_RESET:-0}" = 1 ]; then
    args+=(--reset)
  fi
  require_free_port "${p2p##*:}" "PLAUTHIUM_P2P_LISTEN, e.g. PLAUTHIUM_P2P_LISTEN=0.0.0.0:30334
       (a second public node also needs its own PLAUTHIUM_HOME)"
  require_free_port "${rpc##*:}" "PLAUTHIUM_RPC_LISTEN, e.g. PLAUTHIUM_RPC_LISTEN=127.0.0.1:8090
       (a second public node also needs its own PLAUTHIUM_HOME)"

  mkdir -p "$data_dir"
  echo "============================================================"
  if [ "$MODE" = join ] && [ -n "$bootnodes" ]; then
    echo "Joining Plauthium Public Testnet v1 (mining once connected and caught up)"
  elif [ "$MODE" = join ]; then
    echo "Running Plauthium Public Testnet v1 without bootnodes (mining from the start)"
  else
    echo "Joining Plauthium Public Testnet v1 (verification only)"
  fi
  show_key "$data_dir" "$key_file"
  echo "  Chain:     $data_dir (kept across runs; PLAUTHIUM_RESET=1 wipes it)"
  if [ -n "$bootnodes" ]; then
    echo "  Bootnodes: $bootnodes"
  elif [ "$MODE" = join ]; then
    echo "  Bootnodes: none (mining alone)"
  else
    echo "  Bootnodes: none"
  fi
  echo "  P2P:       $p2p"
  echo "  Explorer:  http://$rpc/"
  echo "Press Ctrl-C to stop."
  echo "============================================================"
  "$BIN" run \
    --data-dir "$data_dir" \
    --key-file "$key_file" \
    --p2p-listen "$p2p" \
    --rpc-listen "$rpc" \
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
  echo "============================================================"
  echo "Plauthium local dev testnet (3 nodes, fresh genesis)"
  for i in 1 2 3; do
    echo "Node $i: explorer http://127.0.0.1:808$i/"
    show_key "$TESTNET_DIR/n$i"
  done
  echo "Jobs become fundable at height 24 (2 x blocks-per-epoch)."
  echo "Press Ctrl-C to stop all nodes."
  echo "============================================================"
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
  join|verify) run_public ;;
  local) run_local ;;
esac

sleep 2
nodes_alive || exit "$NODE_EXIT"
while nodes_alive; do
  sleep 1
done
exit "$NODE_EXIT"
