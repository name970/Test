#!/usr/bin/env bash
# Starts a 3-node Plauthium testnet on localhost (blueprint §10).
#
#   scripts/local_testnet.sh            # from the workspace root
#
# Node i (i = 1..3) listens for peers on 127.0.0.1:3033i, serves its explorer on
# http://127.0.0.1:808i/ and keeps its data in target/testnet/ni. On its first run each
# node generates an ML-DSA-87 miner key in target/testnet/ni/miner_key.bin; later runs
# load it (--reset wipes the chain data but keeps the key). Ctrl-C stops all three nodes.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "Building plauthium-node (release)..."
cargo build --release -p node

BIN="./target/release/plauthium-node"
TESTNET_DIR="./target/testnet"
GENESIS_FILE="$TESTNET_DIR/genesis.json"
mkdir -p "$TESTNET_DIR"

"$BIN" genesis \
  --out "$GENESIS_FILE" \
  --seal-attempts 128 \
  --target-block-secs 5 \
  --blocks-per-epoch 12

PIDS=()
cleanup() {
  trap - INT TERM EXIT
  echo
  echo "Stopping local testnet nodes..."
  for pid in "${PIDS[@]}"; do
    kill -INT "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup INT TERM EXIT

for i in 1 2 3; do
  peers=()
  for j in 1 2 3; do
    if [ "$j" != "$i" ]; then
      peers+=(--peer "127.0.0.1:3033$j")
    fi
  done
  "$BIN" run \
    --data-dir "$TESTNET_DIR/n$i" \
    --genesis "$GENESIS_FILE" \
    --p2p-listen "127.0.0.1:3033$i" \
    --rpc-listen "127.0.0.1:808$i" \
    "${peers[@]}" \
    --mining-threads 1 \
    --reset &
  PIDS+=($!)
done

echo "============================================================"
echo "Plauthium 3-node local testnet running:"
echo "  Node 1 explorer: http://127.0.0.1:8081/"
echo "  Node 2 explorer: http://127.0.0.1:8082/"
echo "  Node 3 explorer: http://127.0.0.1:8083/"
echo "Miner keys: $TESTNET_DIR/n{1,2,3}/miner_key.bin (kept across runs)"
echo "Press Ctrl-C to stop all nodes."
echo "============================================================"
wait
