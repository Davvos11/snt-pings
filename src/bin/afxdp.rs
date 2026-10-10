//! AF_XDP ICMPv6 pixel flooder.
//!
//! Kernel-bypass sender for the Jinglepings canvas. Unlike the plain-socket
//! `snt-pings` binary, AF_XDP hands raw Ethernet frames straight to the NIC's TX
//! queue, so we build the *entire* frame ourselves: Ethernet + IPv6 + ICMPv6,
//! including the ICMPv6 checksum the kernel used to compute for us on the DGRAM
//! socket.
//!
//! Because the canvas is on-link (the MOTD hands you a destination MAC), we just
//! address frames straight at that MAC — no routing, no neighbour lookup.
//!
//! Prereqs on the host:
//!   * find the canvas next-hop MAC *before* running:  `ip -6 neighbor show`
//!   * the interface must be up:                       `ip link set enp6s0 up`
//!   * run as root (AF_XDP needs CAP_NET_RAW + CAP_BPF/privilege).
//!
//! To use all 4 queues/cores, launch one instance per queue, pinned to a core:
//!   for q in 0 1 2 3; do
//!     taskset -c $q ./afxdp img.png 1100 1900 -q $q --dst-mac ac:8f:f8:5c:92:7d &
//!   done

use clap::Parser;
use rand::prelude::SliceRandom;
use std::ffi::CString;
use std::net::Ipv6Addr;
use std::num::NonZeroU32;
use std::ptr::NonNull;

use xdpilone::xdp::XdpDesc;
use xdpilone::{BufIdx, IfInfo, Socket, SocketConfig, Umem, UmemConfig};

/// Full on-wire frame length: 14 (Ethernet) + 40 (IPv6) + 8 (ICMPv6 echo).
const FRAME_LEN: usize = 14 + 40 + 8;
/// Per-frame slot size in the UMEM (power of two, >= FRAME_LEN).
const FRAME_SIZE: u32 = 2048;
/// TX / completion ring depth (power of two).
const RING_SIZE: u32 = 1 << 11;

#[derive(Parser, Debug)]
#[command(version, about = "AF_XDP ICMPv6 pixel flooder", long_about = None)]
struct Args {
    filename: String,
    x: usize,
    y: usize,

    /// Interface to send on.
    #[arg(short, long, default_value = "enp6s0")]
    interface: String,

    /// NIC TX/RX queue id to bind this socket to.
    #[arg(short, long, default_value_t = 0)]
    queue: u32,

    /// Destination MAC (the canvas next-hop, from `ip -6 neighbor show`).
    #[arg(long)]
    dst_mac: String,

    /// Source MAC. Defaults to the interface's own MAC.
    #[arg(long)]
    src_mac: Option<String>,

    /// Source IPv6 address. The canvas ignores it; just needs to not be filtered.
    #[arg(long, default_value = "fe80::1")]
    src_ip: Ipv6Addr,

    /// Resize the image to this width (pixels) before encoding. If --height is
    /// omitted the aspect ratio is preserved.
    #[arg(long)]
    width: Option<u32>,

    /// Resize the image to this height (pixels) before encoding. If --width is
    /// omitted the aspect ratio is preserved.
    #[arg(long)]
    height: Option<u32>,

    /// Force copy mode instead of zero-copy (use if zero-copy bind fails).
    #[arg(long)]
    copy: bool,

    /// Instead of sending, write every built frame (62 bytes each, concatenated)
    /// to this file for the DPDK sender to consume, then exit. Generate this on
    /// the sending host so the source MAC matches the VF (Intel anti-spoofing).
    #[arg(long)]
    dump: Option<String>,
}

fn main() {
    let args = Args::parse();

    let dst_mac = parse_mac(&args.dst_mac).expect("invalid --dst-mac");
    let src_mac = match &args.src_mac {
        Some(s) => parse_mac(s).expect("invalid --src-mac"),
        None => read_iface_mac(&args.interface).expect("could not read interface MAC"),
    };

    // ---- pixel -> destination address encoding (same scheme as main.rs) ----
    // Sniff the format from the file's magic bytes rather than its extension,
    // so extension-less downloads (e.g. from webcam.sh) decode correctly.
    let img = image::ImageReader::open(&args.filename)
        .expect("Failed to open image file")
        .with_guessed_format()
        .expect("Failed to read image header")
        .decode()
        .expect("Failed to decode image");
    let img = match (args.width, args.height) {
        (None, None) => img,
        (w, h) => {
            let (ow, oh) = (img.width() as u64, img.height() as u64);
            assert!(ow > 0 && oh > 0, "source image has zero dimension");
            let (nw, nh) = match (w, h) {
                (Some(w), Some(h)) => (w, h),
                // Preserve aspect when only one dimension is given.
                (Some(w), None) => (w, ((w as u64 * oh) / ow).max(1) as u32),
                (None, Some(h)) => (((h as u64 * ow) / oh).max(1) as u32, h),
                (None, None) => unreachable!(),
            };
            img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
        }
    };
    let rgba_data = img.to_rgba8().into_raw();
    let width = img.width() as usize;

    let mut dst_addrs: Vec<Ipv6Addr> = rgba_data
        .chunks(4)
        .enumerate()
        .filter(|(_, rgba)| rgba[3] > 0)
        .map(|(i, rgba)| {
            let x = args.x + (i % width);
            let y = args.y + (i / width);
            Ipv6Addr::new(
                0x2001,
                0x610,
                0x5ea,
                0x221e,
                x as u16,
                y as u16,
                (rgba[2] as u16) << 8 | rgba[1] as u16,
                (rgba[0] as u16) << 8 | rgba[3] as u16,
            )
        })
        .collect();

    dst_addrs.shuffle(&mut rand::rng());
    let n = dst_addrs.len();
    assert!(n > 0, "no visible pixels to send");
    println!("pixels: {n}");

    // Dump mode: write all complete frames for the DPDK sender, then exit.
    if let Some(path) = &args.dump {
        let mut out = Vec::with_capacity(n * FRAME_LEN);
        for dst in &dst_addrs {
            out.extend_from_slice(&build_frame(&dst_mac, &src_mac, &args.src_ip, dst));
        }
        std::fs::write(path, &out).expect("failed to write dump file");
        println!("wrote {n} frames ({} bytes) to {path}", out.len());
        return;
    }

    // One UMEM frame per pixel: preload each frame once, then recirculate the
    // descriptors forever — zero per-packet work in the hot loop. Needs
    // frame_count >= n.
    let frame_count = (n as u32).next_power_of_two().max(RING_SIZE);

    // ---- allocate page-aligned UMEM via mmap ----
    let umem_size = frame_count as usize * FRAME_SIZE as usize;
    let mem = alloc_umem(umem_size);

    // SAFETY: `mem` is a fresh, exclusively-owned, page-aligned mmap of umem_size
    // bytes that lives for the rest of the process (never unmapped).
    let umem = unsafe {
        Umem::new(
            UmemConfig {
                fill_size: RING_SIZE,
                complete_size: RING_SIZE,
                frame_size: FRAME_SIZE,
                headroom: 0,
                flags: 0,
            },
            mem,
        )
    }
    .expect("failed to create UMEM");

    // ---- bind an AF_XDP socket to (interface, queue) ----
    let mut iface = IfInfo::invalid();
    let ifname = CString::new(args.interface.clone()).unwrap();
    iface
        .from_name(&ifname)
        .expect("interface not found");
    iface.set_queue(args.queue);

    let sock = Socket::with_shared(&iface, &umem).expect("failed to create socket");
    let mut device = umem.fq_cq(&sock).expect("failed to get fill/completion rings");

    // Native zero-copy is the fast path; fall back to native copy with --copy if
    // the driver can't do ZC. (Plain generic/SKB mode is the slow path we're
    // avoiding — that's what capped us at ~29k pps.)
    let mode_flag = if args.copy {
        SocketConfig::XDP_BIND_COPY
    } else {
        SocketConfig::XDP_BIND_ZEROCOPY
    };
    let rxtx = umem
        .rx_tx(
            &sock,
            &SocketConfig {
                rx_size: None,
                tx_size: NonZeroU32::new(RING_SIZE),
                bind_flags: SocketConfig::XDP_BIND_NEED_WAKEUP | mode_flag,
            },
        )
        .expect("failed to set up rx/tx rings (try --copy if zero-copy is unsupported)");
    let mut tx = rxtx.map_tx().expect("failed to map tx ring");

    umem.bind(&rxtx).expect("bind failed");

    // ---- preload every frame with its fully-built packet ----
    // `free` holds the descriptors we currently own and may (re)transmit.
    let mut free: Vec<XdpDesc> = Vec::with_capacity(n);
    for (i, dst) in dst_addrs.iter().enumerate() {
        let bytes = build_frame(&dst_mac, &src_mac, &args.src_ip, dst);
        let mut frame = umem.frame(BufIdx(i as u32)).expect("frame out of range");
        // SAFETY: each BufIdx maps to a distinct, non-overlapping FRAME_SIZE slot
        // in our umem; we write only the first FRAME_LEN bytes.
        unsafe {
            frame.addr.as_mut()[..FRAME_LEN].copy_from_slice(&bytes);
        }
        free.push(XdpDesc {
            addr: frame.offset,
            len: FRAME_LEN as u32,
            options: 0,
        });
    }

    println!(
        "flooding {} queue {} -> {} ({} frames)",
        args.interface, args.queue, args.dst_mac, frame_count
    );

    // ---- hot loop: recirculate descriptors through TX + completion rings ----
    let mut sent: u64 = 0;
    let mut last = std::time::Instant::now();
    loop {
        if !free.is_empty() {
            let mut writer = tx.transmit(free.len() as u32);
            // insert() consumes from the iterator up to the ring's free space and
            // returns how many it actually queued.
            let inserted = writer.insert(free.iter().copied()) as usize;
            writer.commit();
            free.drain(0..inserted);
        }

        if tx.needs_wakeup() {
            tx.wake();
        }

        // Reclaim completed frames: each returns the umem address we submitted.
        let mut reader = device.complete(RING_SIZE);
        while let Some(addr) = reader.read() {
            free.push(XdpDesc {
                addr,
                len: FRAME_LEN as u32,
                options: 0,
            });
            sent += 1;
        }
        reader.release();

        // Report throughput roughly once a second.
        if last.elapsed().as_secs() >= 1 {
            let secs = last.elapsed().as_secs_f64();
            println!("{:.2} Mpps", sent as f64 / secs / 1e6);
            sent = 0;
            last = std::time::Instant::now();
        }
    }
}

/// mmap an anonymous, page-aligned region for the UMEM.
fn alloc_umem(size: usize) -> NonNull<[u8]> {
    // SAFETY: standard anonymous mmap; we check for MAP_FAILED.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(ptr != libc::MAP_FAILED, "umem mmap failed");
    let slice = std::ptr::slice_from_raw_parts_mut(ptr as *mut u8, size);
    NonNull::new(slice).unwrap()
}

/// Build one complete Ethernet + IPv6 + ICMPv6 Echo Request frame.
fn build_frame(
    dst_mac: &[u8; 6],
    src_mac: &[u8; 6],
    src_ip: &Ipv6Addr,
    dst_ip: &Ipv6Addr,
) -> [u8; FRAME_LEN] {
    let mut f = [0u8; FRAME_LEN];
    let src = src_ip.octets();
    let dst = dst_ip.octets();

    // Ethernet header.
    f[0..6].copy_from_slice(dst_mac);
    f[6..12].copy_from_slice(src_mac);
    f[12..14].copy_from_slice(&0x86DDu16.to_be_bytes()); // IPv6

    // IPv6 header.
    f[14] = 0x60; // version 6, traffic class 0
    // f[15..18] flow label = 0 (already zero)
    f[18..20].copy_from_slice(&8u16.to_be_bytes()); // payload length = 8
    f[20] = 58; // next header = ICMPv6
    f[21] = 64; // hop limit
    f[22..38].copy_from_slice(&src);
    f[38..54].copy_from_slice(&dst);

    // ICMPv6 Echo Request (checksum left 0 for now).
    f[54] = 128; // type: Echo Request
    f[55] = 0; // code
    f[58..60].copy_from_slice(&1u16.to_be_bytes()); // identifier
    f[60..62].copy_from_slice(&1u16.to_be_bytes()); // sequence

    // ICMPv6 checksum over the IPv6 pseudo-header + the 8-byte message.
    let csum = icmpv6_checksum(&src, &dst, &f[54..62]);
    f[56..58].copy_from_slice(&csum.to_be_bytes());
    f
}

/// RFC 4443 ICMPv6 checksum: ones-complement sum over the IPv6 pseudo-header
/// (src, dst, upper-layer length, next header) plus the ICMPv6 message (with its
/// checksum field zeroed).
fn icmpv6_checksum(src: &[u8; 16], dst: &[u8; 16], msg: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for part in [src.as_slice(), dst.as_slice(), msg] {
        for pair in part.chunks(2) {
            let hi = pair[0] as u32;
            let lo = *pair.get(1).unwrap_or(&0) as u32;
            sum += (hi << 8) | lo;
        }
    }
    sum += msg.len() as u32; // upper-layer packet length (8)
    sum += 58; // next header = ICMPv6 (the 3 preceding bytes are zero)

    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Parse "aa:bb:cc:dd:ee:ff" into 6 bytes.
fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(mac)
}

/// Read a NIC's MAC from /sys/class/net/<iface>/address.
fn read_iface_mac(iface: &str) -> Option<[u8; 6]> {
    let s = std::fs::read_to_string(format!("/sys/class/net/{iface}/address")).ok()?;
    parse_mac(s.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_is_valid() {
        // A correct ICMPv6 checksum makes the receiver's own sum over
        // pseudo-header + message (including the checksum field) fold to 0xFFFF.
        let src = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1).octets();
        let dst =
            Ipv6Addr::new(0x2001, 0x610, 0x5ea, 0x221e, 0x10, 0x20, 0x3040, 0x5060).octets();
        let f = build_frame(
            &[1, 2, 3, 4, 5, 6],
            &[7, 8, 9, 10, 11, 12],
            &src.into(),
            &dst.into(),
        );

        let mut sum: u32 = 0;
        for pair in src.chunks(2).chain(dst.chunks(2)).chain(f[54..62].chunks(2)) {
            sum += ((pair[0] as u32) << 8) | pair[1] as u32;
        }
        sum += 8 + 58;
        while (sum >> 16) != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        assert_eq!(sum as u16, 0xffff);
    }
}
