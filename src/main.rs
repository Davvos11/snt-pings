use clap::Parser;
use rand::prelude::SliceRandom;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::Ipv6Addr;
use std::os::fd::AsRawFd;
use std::thread;
use std::thread::sleep;
use std::time::Duration;

const PACKET_SIZE: f32 = 8.0 + 40.0 + 14.0; // ICMP + IPv6 + Ethernet (in bytes)

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    filename: String,
    x: usize,
    y: usize,
    /// Target bitrate in Mb/s. NOTE: with the sendmmsg path this is now a *coarse*
    /// per-batch throttle (we sleep once per BATCH packets), not a per-packet one.
    /// Leave it unset for maximum throughput, which is the default we want.
    #[arg(short, long)]
    mbps: Option<f32>,
    #[arg(short, long, default_value = "1")]
    threads: usize,
    /// Packets per sendmmsg syscall (batch size).
    #[arg(short, long, default_value_t = 1024)]
    batch: usize,
}

fn main() {
    let args = Args::parse();

    let img = image::open(args.filename).expect("Failed to open image");
    let rgba_data = img.to_rgba8().into_raw();
    let width = img.width() as usize;

    // Collect the target addresses directly as Ipv6Addr; we build the raw
    // sockaddr_in6 structs ourselves per thread in run().
    let addresses: Vec<Ipv6Addr> = rgba_data
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

    dbg!(addresses.len());

    let batch = args.batch;
    assert!(batch > 0, "--batch must be at least 1");

    // Coarse per-batch sleep derived from the requested bitrate: one sleep covers a
    // whole batch of packets, so the resulting rate is approximate (but good enough
    // for throttling a flood we mostly want to run flat out).
    let batch_sleep: Option<Duration> = args.mbps.map(|mbps| {
        let packets_per_sec = (mbps * 1_000_000.0) / 8.0 / PACKET_SIZE;
        Duration::from_secs_f32(batch as f32 / packets_per_sec)
    });

    let threads: Vec<_> = (0..args.threads)
        .map(|_| {
            let addresses = addresses.clone();
            thread::spawn(move || run(addresses, batch, batch_sleep))
        })
        .collect();

    for thread in threads {
        thread.join().unwrap();
    }
}

fn run(addresses: Vec<Ipv6Addr>, batch: usize, batch_sleep: Option<Duration>) {
    let mut rng = rand::rng();
    let mut addresses = addresses;
    addresses.shuffle(&mut rng);

    let n = addresses.len();
    assert!(n > 0, "no visible pixels to send");

    // ICMPv6 Echo Request payload. Checksum bytes stay 0: a DGRAM ICMPv6 socket
    // makes the kernel compute the checksum for us.
    // SAFETY INVARIANT: `packet` lives for the whole function, so the iovec below
    // (which points into it) never dangles.
    let packet: [u8; 8] = [128, 0, 0, 0, 0, 1, 0, 1];

    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::ICMPV6))
        .expect("Could not open socket");
    // Bigger send buffer => fewer stalls when the kernel drains slower than we fill.
    let _ = socket.set_send_buffer_size(16 * 1024 * 1024);
    let fd = socket.as_raw_fd();

    // SAFETY INVARIANT: `sockaddrs` is allocated once with its final capacity and is
    // never pushed to, moved, or reallocated after the mmsghdrs below are built. Each
    // `mmsghdr.msg_hdr.msg_name` holds a raw pointer into this Vec's buffer, so the
    // buffer must stay at a fixed address and stay alive for the whole function.
    let mut sockaddrs: Vec<libc::sockaddr_in6> = Vec::with_capacity(n);
    for addr in &addresses {
        // sockaddr_in6 is plain-old-data; zero it, then fill the fields we use.
        let mut sa: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        sa.sin6_addr.s6_addr = addr.octets();
        sockaddrs.push(sa);
    }
    // Take the base pointer only after the Vec is fully populated and will not grow.
    let sa_base = sockaddrs.as_mut_ptr();
    let namelen = std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;

    // A single iovec shared by every message: all packets carry the same 8 bytes.
    // SAFETY INVARIANT: `packet` and `iov` both outlive every sendmmsg call below.
    let mut iov = libc::iovec {
        iov_base: packet.as_ptr() as *mut libc::c_void,
        iov_len: packet.len(),
    };
    let iov_ptr = &mut iov as *mut libc::iovec;

    // One mmsghdr per address. msg_name aliases into `sockaddrs` (never reallocated),
    // msg_iov aliases the single shared `iov` (lives as long as this function).
    let mut msgs: Vec<libc::mmsghdr> = Vec::with_capacity(n);
    for i in 0..n {
        let mut hdr: libc::mmsghdr = unsafe { std::mem::zeroed() };
        // SAFETY: i < n == sockaddrs.len(), so sa_base.add(i) is in-bounds and the
        // pointee stays valid for the whole function per the invariant above.
        hdr.msg_hdr.msg_name = unsafe { sa_base.add(i) } as *mut libc::c_void;
        hdr.msg_hdr.msg_namelen = namelen;
        hdr.msg_hdr.msg_iov = iov_ptr;
        hdr.msg_hdr.msg_iovlen = 1;
        msgs.push(hdr);
    }

    // Cycle forever, sending all messages in batch-sized chunks.
    loop {
        for chunk in msgs.chunks_mut(batch) {
            let mut sent = 0usize;
            while sent < chunk.len() {
                // SAFETY: `chunk[sent..]` is a valid, contiguous, mutable run of
                // `mmsghdr`; `fd` is a live ICMPv6 socket; every pointer inside each
                // header aliases memory (`sockaddrs`, `packet`/`iov`) that is still
                // alive and at a fixed address. sendmmsg reads these structs and only
                // writes each header's `msg_len` field.
                let ret = unsafe {
                    libc::sendmmsg(
                        fd,
                        chunk.as_mut_ptr().add(sent),
                        (chunk.len() - sent) as libc::c_uint,
                        0,
                    )
                };
                if ret < 0 {
                    let err = std::io::Error::last_os_error();
                    match err.raw_os_error() {
                        // Backpressure: the send buffer / qdisc is momentarily full.
                        // Wait until the socket is writable again instead of sleeping a
                        // fixed, unreliable interval (thread::sleep oversleeps badly for
                        // tiny durations under load). poll() returns the instant there's
                        // space, so we neither busy-spin nor stall. 1s timeout is just a
                        // safety cap before we retry regardless.
                        Some(libc::EAGAIN) | Some(libc::ENOBUFS) => {
                            let mut pfd = libc::pollfd {
                                fd,
                                events: libc::POLLOUT,
                                revents: 0,
                            };
                            // SAFETY: &mut pfd is one valid pollfd for the call's duration.
                            unsafe {
                                libc::poll(&mut pfd as *mut libc::pollfd, 1, 1000);
                            }
                        }
                        // Anything else is unexpected: log, back off longer, retry.
                        _ => {
                            println!("sendmmsg failed: {err}");
                            sleep(Duration::from_millis(100));
                        }
                    }
                    continue;
                }
                // Partial send: advance by however many the kernel accepted.
                sent += ret as usize;
            }

            if let Some(d) = batch_sleep {
                sleep(d);
            }
        }
    }
}
