#!/usr/bin/env bash
#
# Flood the Jinglepings canvas via the DPDK sender with one or more images.
#
# Usage:
#   ./flood.sh IMAGE:X:Y [IMAGE:X:Y ...]
#
# Core split: the first N-1 images get 1 core each, the LAST image gets all the
# remaining cores. With a single image, every core floods it.
#
# Examples:
#   ./flood.sh vck-anti-serv.png:1100:1900
#   ./flood.sh logo.png:1100:1900 bg.png:0:0        # logo on 1 core, bg on the rest
#
# Host-specific values (MAC/IP/PCI/cores) can be overridden via env vars, e.g.:
#   SRC_IP=fd00:dead:beef:0:... CORES=0-7 ./flood.sh img.png:0:0
#
set -euo pipefail

# ---- config (override via environment) ----
: "${DST_MAC:=ac:8f:f8:5c:92:bf}"                    # canvas next-hop MAC
: "${SRC_MAC:=10:66:6a:0d:d5:ae}"                    # this VF's MAC (anti-spoofing)
: "${SRC_IP:=fd00:dead:beef:0:1266:6aff:fe0d:d5ae}"  # routable source address
: "${PCI:=0000:06:00.0}"                             # VF PCI address (vfio-pci)
: "${CORES:=0-3}"                                     # EAL lcore list
: "${MEM_CHANNELS:=4}"                               # EAL -n

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AFXDP="$ROOT/target/release/afxdp"
PINGFLOOD="$ROOT/dpdk/pingflood"
FRAMEDIR="$ROOT/frames"

if [[ $# -lt 1 ]]; then
    echo "usage: $0 IMAGE:X:Y [IMAGE:X:Y ...]" >&2
    exit 1
fi

# Build if needed.
[[ -x "$AFXDP" ]] || { echo ">> building afxdp"; (cd "$ROOT" && cargo build --release --bin afxdp); }
[[ -x "$PINGFLOOD" ]] || { echo ">> building pingflood"; make -C "$ROOT/dpdk"; }

mkdir -p "$FRAMEDIR"

# Generate one frame file per image spec.
files=()
i=0
for spec in "$@"; do
    IFS=':' read -r img x y <<< "$spec"
    if [[ -z "${img:-}" || -z "${x:-}" || -z "${y:-}" ]]; then
        echo "bad spec '$spec' (expected IMAGE:X:Y)" >&2
        exit 1
    fi
    # Resolve image relative to cwd, else relative to the repo root.
    if [[ ! -f "$img" && -f "$ROOT/$img" ]]; then
        img="$ROOT/$img"
    fi
    [[ -f "$img" ]] || { echo "image not found: $img" >&2; exit 1; }

    out="$FRAMEDIR/frames_$i.bin"
    echo ">> $img at ($x,$y) -> $out"
    "$AFXDP" "$img" "$x" "$y" \
        --dst-mac "$DST_MAC" --src-mac "$SRC_MAC" --src-ip "$SRC_IP" \
        --dump "$out"
    files+=("$out")
    i=$((i + 1))
done

echo ">> stopping any running pingflood"
sudo pkill -f "$PINGFLOOD" 2>/dev/null || true

if [[ ${#files[@]} -eq 1 ]]; then
    echo ">> flooding 1 image across all cores ($CORES)"
else
    echo ">> flooding: first $((${#files[@]} - 1)) image(s) on 1 core each, last on the rest"
fi

exec sudo "$PINGFLOOD" -l "$CORES" -n "$MEM_CHANNELS" -a "$PCI" -- "${files[@]}"
