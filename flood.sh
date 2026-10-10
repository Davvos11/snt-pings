#!/usr/bin/env bash
#
# Flood the Jinglepings canvas via the DPDK sender with one or more images.
#
# Usage:
#   ./flood.sh ITEM [ITEM ...]
#
# Each ITEM is one of:
#   IMAGE:X:Y          a static image to place at canvas (X,Y)
#   IMAGE:X:Y:SIZE     ... resized first; SIZE is WxH (exact), W (keep aspect)
#                          or xH (keep aspect)
#   FRAMES.bin         a pre-built frame file (no ':'), passed through and
#                          WATCHED for live reload -- e.g. one kept fresh by
#                          webcam.sh. Produce it separately; flood.sh won't.
#
# Core split: the first N-1 items get 1 core each, the LAST item gets all the
# remaining cores. With a single item, every core floods it.
#
# Examples:
#   ./flood.sh vck-anti-serv.png:1100:1900
#   ./flood.sh logo.png:0:0:400 bg.png:0:0            # logo 400px wide, bg full
#   ./flood.sh ~/border.png:2100:1900 ./frames/webcam.bin ~/yellow.png:1618:763
#
# Host-specific values (MAC/IP/PCI/cores) can be overridden via env vars, e.g.:
#   SRC_IP=fd00:dead:beef:0:... CORES=0-7 ./flood.sh img.png:0:0
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
    echo "usage: $0 ITEM [ITEM ...]   (ITEM = IMAGE:X:Y[:SIZE] or FRAMES.bin)" >&2
    exit 1
fi

# Build if needed.
[[ -x "$AFXDP" ]] || { echo ">> building afxdp"; (cd "$ROOT" && cargo build --release --bin afxdp); }
[[ -x "$PINGFLOOD" ]] || { echo ">> building pingflood"; make -C "$ROOT/dpdk"; }

mkdir -p "$FRAMEDIR"

# Resolve a path relative to cwd, else relative to the repo root.
resolve() {
    local p="$1"
    if [[ ! -e "$p" && -e "$ROOT/$p" ]]; then p="$ROOT/$p"; fi
    printf '%s' "$p"
}

# Build the pingflood argument list, in order.
args=()
i=0
for item in "$@"; do
    if [[ "$item" == *:* ]]; then
        # IMAGE:X:Y[:SIZE] -> dump to a frame file.
        IFS=':' read -r img x y size <<< "$item"
        if [[ -z "${img:-}" || -z "${x:-}" || -z "${y:-}" ]]; then
            echo "bad spec '$item' (expected IMAGE:X:Y[:SIZE])" >&2
            exit 1
        fi
        img="$(resolve "$img")"
        [[ -f "$img" ]] || { echo "image not found: $img" >&2; exit 1; }

        resize_args=()
        if [[ -n "${size:-}" ]]; then
            case "$size" in
                *x*) w="${size%x*}"; h="${size#*x}" ;;
                *)   w="$size"; h="" ;;
            esac
            [[ -n "$w" ]] && resize_args+=(--width  "$w")
            [[ -n "$h" ]] && resize_args+=(--height "$h")
        fi

        out="$FRAMEDIR/frames_$i.bin"
        echo ">> $img at ($x,$y)${size:+ size $size} -> $out"
        "$AFXDP" "$img" "$x" "$y" \
            --dst-mac "$DST_MAC" --src-mac "$SRC_MAC" --src-ip "$SRC_IP" \
            ${resize_args[@]+"${resize_args[@]}"} --dump "$out"
        args+=("$out")
    else
        # Pre-built frame file: pass through and WATCH it for live reload.
        bin="$(resolve "$item")"
        [[ -f "$bin" ]] || { echo "frames file not found: $bin (run webcam.sh first?)" >&2; exit 1; }
        echo ">> watching pre-built frames: $bin"
        args+=("@$bin")
    fi
    i=$((i + 1))
done

echo ">> stopping any running pingflood"
sudo pkill -f "$PINGFLOOD" 2>/dev/null || true

if [[ ${#args[@]} -eq 1 ]]; then
    echo ">> flooding 1 item across all cores ($CORES)"
else
    echo ">> flooding: first $((${#args[@]} - 1)) item(s) on 1 core each, last on the rest"
fi

exec sudo "$PINGFLOOD" -l "$CORES" -n "$MEM_CHANNELS" -a "$PCI" -- "${args[@]}"
