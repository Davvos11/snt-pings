#!/usr/bin/env bash
#
# Poll a changing web image (e.g. a webcam JPEG) and keep a .bin frame file
# up to date for pingflood's watch mode.
#
# Usage:
#   ./webcam.sh URL X Y [OUT.bin] [INTERVAL_SECONDS] [SIZE]
#
# SIZE scales the image on the canvas: WxH (exact), W (width, keep aspect),
# or xH (height, keep aspect). May also be set via WIDTH/HEIGHT env vars.
#
# Example (place the webcam at canvas 800,600, 400px wide, refresh every 1s):
#   ./webcam.sh 'https://www.vestingbar.nl/webcam-images/image.jpg' 800 600 '' 1 400
#
# Then, in another shell, flood it with live reload (note the '@' = watch):
#   sudo dpdk/pingflood -l 0-3 -n 4 -a 0000:06:00.0 -- @frames/webcam.bin
#
# This downloads the image, converts it to 62-byte frames with `afxdp --dump`,
# and atomically renames the result over OUT so pingflood never sees a
# half-written file. MAC/IP/PCI come from the same env vars as flood.sh.
set -euo pipefail

# ---- config (override via environment, same defaults as flood.sh) ----
: "${DST_MAC:=ac:8f:f8:5c:92:bf}"                    # canvas next-hop MAC
: "${SRC_MAC:=10:66:6a:0d:d5:ae}"                    # this VF's MAC (anti-spoofing)
: "${SRC_IP:=fd00:dead:beef:0:1266:6aff:fe0d:d5ae}"  # routable source address

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AFXDP="$ROOT/target/release/afxdp"

URL="${1:?usage: $0 URL X Y [OUT.bin] [INTERVAL_SECONDS]}"
X="${2:?need X}"
Y="${3:?need Y}"
OUT="${4:-$ROOT/frames/webcam.bin}"
INTERVAL="${5:-1}"
SIZE="${6:-}"

# Resolve size: positional SIZE (WxH / W / xH) overrides WIDTH/HEIGHT env vars.
: "${WIDTH:=}"
: "${HEIGHT:=}"
if [[ -n "$SIZE" ]]; then
    case "$SIZE" in
        *x*) WIDTH="${SIZE%x*}"; HEIGHT="${SIZE#*x}" ;;
        *)   WIDTH="$SIZE" ;;
    esac
fi
resize_args=()
[[ -n "$WIDTH"  ]] && resize_args+=(--width  "$WIDTH")
[[ -n "$HEIGHT" ]] && resize_args+=(--height "$HEIGHT")

# Build afxdp if needed (we only use its --dump path; no NIC touched here).
[[ -x "$AFXDP" ]] || { echo ">> building afxdp"; (cd "$ROOT" && cargo build --release --bin afxdp); }

mkdir -p "$(dirname "$OUT")"

# Strip any existing query string; we add our own cache-buster each poll.
BASE_URL="${URL%%\?*}"

TMPDIR="$(mktemp -d)"
cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT
trap 'echo; echo ">> stopping webcam poller"; exit 0' INT TERM

echo ">> polling $BASE_URL -> $OUT at ($X,$Y) every ${INTERVAL}s"
echo ">> flood it with: sudo $ROOT/dpdk/pingflood -l 0-3 -n 4 -a <PCI> -- @$OUT"

jpg="$TMPDIR/img"            # extension-less; afxdp sniffs the format
while true; do
    # Cache-buster so we always get the freshest frame.
    if curl -fsS --max-time "$INTERVAL" -o "$jpg" "${BASE_URL}?t=$(date +%s%N)"; then
        # Write frames next to OUT so the rename is atomic (same filesystem).
        tmpbin="$(mktemp "${OUT}.XXXXXX")"
        if "$AFXDP" "$jpg" "$X" "$Y" \
               --dst-mac "$DST_MAC" --src-mac "$SRC_MAC" --src-ip "$SRC_IP" \
               ${resize_args[@]+"${resize_args[@]}"} --dump "$tmpbin" >/dev/null 2>&1; then
            mv -f "$tmpbin" "$OUT"
        else
            echo ">> convert failed (bad/empty image?), keeping previous frames" >&2
            rm -f "$tmpbin"
        fi
    else
        echo ">> download failed, keeping previous frames" >&2
    fi
    sleep "$INTERVAL"
done
