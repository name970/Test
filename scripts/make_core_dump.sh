#!/usr/bin/env bash
# Rebuilds plauthium_core_dump.txt from a local plauthium-workspace checkout.
#
#   scripts/make_core_dump.sh <workspace-root> [output-file]
#
# Emits the workspace files (Cargo.toml, Cargo.lock, .gitignore) and, for every
# local crate listed in Cargo.lock, its Cargo.toml and every .rs file under
# src/ (lib.rs and main.rs first). Expected files that do not exist are listed
# in the manifest as MISSING, so a partial checkout is visible at a glance.
set -euo pipefail

root=${1:?usage: make_core_dump.sh <workspace-root> [output-file]}
out=${2:-plauthium_core_dump.txt}
root=${root%/}
# Dependency order of the core (M1..M5 + PoC); other local crates follow alphabetically.
preferred="primitives work scivm scheduler ledger poc"

[ -f "$root/Cargo.lock" ] || { echo "error: $root/Cargo.lock not found" >&2; exit 1; }

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# Local packages are the [[package]] entries of Cargo.lock without a `source` line.
local_crates() {
  awk '/^\[\[package\]\]/ { if (name != "" && !src) print name; name = ""; src = 0 }
       /^name = /         { name = $3; gsub(/"/, "", name) }
       /^source = /       { src = 1 }
       END                { if (name != "" && !src) print name }' "$root/Cargo.lock" | sort
}

crate_dir() {
  for d in "$root/$1" "$root/crates/$1"; do
    [ -d "$d" ] && { echo "${d#"$root"/}"; return; }
  done
  echo "$1"
}

all=$(local_crates)
crates=""
for c in $preferred; do
  if echo "$all" | grep -qx "$c"; then crates="$crates $c"; fi
done
for c in $all; do
  case " $preferred " in *" $c "*) ;; *) crates="$crates $c" ;; esac
done

# Ordered entries "path|required" (required=1 means report MISSING when absent).
entries="Cargo.toml|1
Cargo.lock|1
.gitignore|0
rust-toolchain.toml|0
rustfmt.toml|0
clippy.toml|0"
for c in $crates; do
  dir=$(crate_dir "$c")
  entries="$entries
$dir/Cargo.toml|1"
  if [ -f "$root/$dir/src/lib.rs" ] || [ -f "$root/$dir/src/main.rs" ]; then
    for f in lib.rs main.rs; do
      [ -f "$root/$dir/src/$f" ] && entries="$entries
$dir/src/$f|1"
    done
    if [ -d "$root/$dir/src" ]; then
      for f in $(cd "$root/$dir" && find src -name '*.rs' ! -path src/lib.rs ! -path src/main.rs | LC_ALL=C sort); do
        entries="$entries
$dir/$f|1"
      done
    fi
  else
    entries="$entries
$dir/src/lib.rs (or src/main.rs)|1"
  fi
done

present=0
missing=0
manifest=""
while IFS='|' read -r path required; do
  if [ -f "$root/$path" ]; then
    present=$((present + 1))
    lines=$(wc -l < "$root/$path" | tr -d ' ')
    manifest="$manifest
  PRESENT  $(printf '%-34s' "$path") $(printf '%5s' "$lines") lines  sha256 $(sha256 "$root/$path")"
  elif [ "$required" = 1 ]; then
    missing=$((missing + 1))
    manifest="$manifest
  MISSING  $path"
  fi
done <<EOF
$entries
EOF

rule=$(printf '#%.0s' $(seq 1 80))
{
  echo "$rule"
  echo "PLAUTHIUM CORE DUMP"
  echo "format:    plauthium-core-dump/1"
  echo "workspace: $(basename "$root")"
  echo "crates:   $crates"
  echo "generated: $(date -u +%Y-%m-%dT%H:%M:%SZ) by scripts/make_core_dump.sh"
  echo "files:     $present present, $missing missing"
  echo
  echo "Each file appears between the markers"
  echo "  ##### BEGIN FILE: <path> | <n> lines | sha256 <hex>"
  echo "  ##### END FILE: <path>"
  echo "Content between the markers is verbatim. Paths are relative to the workspace root."
  echo "$rule"
  echo
  echo "MANIFEST$manifest"
  echo
  while IFS='|' read -r path _; do
    [ -f "$root/$path" ] || continue
    echo "$rule"
    echo "##### BEGIN FILE: $path | $(wc -l < "$root/$path" | tr -d ' ') lines | sha256 $(sha256 "$root/$path")"
    echo "$rule"
    cat "$root/$path"
    [ -n "$(tail -c1 "$root/$path")" ] && echo
    echo "##### END FILE: $path"
    echo
  done <<EOF
$entries
EOF
} > "$out"

echo "wrote $out ($present present, $missing missing)" >&2
