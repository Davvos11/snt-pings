use clap::Parser;
use rand::prelude::SliceRandom;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use spin_sleep::SpinSleeper;
use std::net::{Ipv6Addr, SocketAddr};
use std::thread;
use std::thread::sleep;
use std::time::{Duration, Instant};

const PACKET_SIZE: f32 = 8.0 + 40.0 + 14.0; // ICMP + IPv6 + Ethernet (in bytes)

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    filename: String,
    x: usize,
    y: usize,
    /// Target bitrate in Mb/s
    #[arg(short, long)]
    mbps: Option<f32>,
    #[arg(short, long, default_value = "1")]
    threads: usize,
    /// Cycle all fully-black pixels through the colours of the rainbow
    #[arg(long)]
    rainbow: bool,
    /// Rainbow cycle speed in hue degrees per second
    #[arg(long, default_value = "60")]
    rainbow_speed: f32,
}

/// A single pixel to be drawn. `addr` is the pre-built address for its static
/// colour; when rainbow mode is on, black pixels get a fresh address each send.
struct Pixel {
    x: u16,
    y: u16,
    alpha: u8,
    is_black: bool,
    addr: SockAddr,
}

/// Pack a pixel position + RGBA colour into the target IPv6 address.
fn make_sockaddr(x: u16, y: u16, rgba: [u8; 4]) -> SockAddr {
    let addr = Ipv6Addr::new(
        0x2001,
        0x610,
        0x5ea,
        0x221e,
        x,
        y,
        (rgba[2] as u16) << 8 | rgba[1] as u16,
        (rgba[0] as u16) << 8 | rgba[3] as u16,
    );
    SockAddr::from(SocketAddr::new(addr.into(), 0))
}

/// Brightness of the rainbow colours (0.0 = black, 1.0 = full). Kept low so the
/// cycling colours stay dark/muted.
const RAINBOW_VALUE: f32 = 0.45;

/// Convert a hue (degrees) at full saturation to a dark RGB triplet.
fn hue_to_rgb(h: f32) -> [u8; 3] {
    let h = h.rem_euclid(360.0) / 60.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    let v = RAINBOW_VALUE * 255.0;
    [(r * v) as u8, (g * v) as u8, (b * v) as u8]
}

fn main() {
    let args = Args::parse();

    let img = image::open(args.filename).expect("Failed to open image");
    let rgba_data = img.to_rgba8().into_raw();
    let width = img.width() as usize;

    let pixels: Vec<Pixel> = rgba_data
        .chunks(4)
        .enumerate()
        .filter(|(_, rgba)| rgba[3] > 0)
        .map(|(i, rgba)| {
            let x = (args.x + (i % width)) as u16;
            let y = (args.y + (i / width)) as u16;
            let rgba = [rgba[0], rgba[1], rgba[2], rgba[3]];
            let is_black = rgba[0] == 0 && rgba[1] == 0 && rgba[2] == 0;
            Pixel {
                x,
                y,
                alpha: rgba[3],
                is_black,
                addr: make_sockaddr(x, y, rgba),
            }
        })
        .collect();

    dbg!(pixels.len());

    let interval: Option<Duration> = args.mbps.map(|mbps| {
        let packets_per_sec = (mbps * 1_000_000.0) / 8.0 / PACKET_SIZE;
        Duration::from_secs_f32(1.0 / packets_per_sec)
    });

    let pixels = std::sync::Arc::new(pixels);
    let rainbow = args.rainbow;
    let rainbow_speed = args.rainbow_speed;

    let threads: Vec<_> = (0..args.threads)
        .map(|_| {
            let pixels = pixels.clone();
            thread::spawn(move || run(&pixels, interval, rainbow, rainbow_speed))
        })
        .collect();

    for thread in threads {
        thread.join().unwrap();
    }
}

fn run(pixels: &[Pixel], interval: Option<Duration>, rainbow: bool, rainbow_speed: f32) {
    let mut rng = rand::rng();
    let mut order: Vec<usize> = (0..pixels.len()).collect();
    order.shuffle(&mut rng);

    // Create an ICMPv6 Echo Request packet
    let mut packet = [0u8; 8];
    packet[0] = 128; // Type: 128 (Echo Request)
    packet[1] = 0; // Code: 0 (no special code)
    packet[2] = 0; // Checksum: 0 for now, can be calculated later if needed
    packet[3] = 0; // Checksum (high byte)
    packet[4] = 0; // Identifier (low byte, arbitrary)
    packet[5] = 1; // Identifier (high byte, arbitrary)
    packet[6] = 0; // Sequence Number (low byte)
    packet[7] = 1; // Sequence Number (high byte)

    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::ICMPV6))
        .expect("Could not open socket");

    let sleeper = SpinSleeper::default();
    let mut next = Instant::now();
    let start = Instant::now();

    for &idx in order.iter().cycle() {
        let pixel = &pixels[idx];

        // For black pixels in rainbow mode, build a fresh address whose colour
        // depends on the elapsed time so it cycles through the rainbow.
        let dynamic;
        let address: &SockAddr = if rainbow && pixel.is_black {
            let hue = start.elapsed().as_secs_f32() * rainbow_speed;
            let rgb = hue_to_rgb(hue);
            dynamic = make_sockaddr(pixel.x, pixel.y, [rgb[0], rgb[1], rgb[2], pixel.alpha]);
            &dynamic
        } else {
            &pixel.addr
        };

        loop {
            match socket.send_to(&packet, address) {
                Ok(_) => break,
                // Send buffer full: back off very briefly and retry the same address.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    sleeper.sleep(Duration::from_micros(50));
                }
                Err(e) => {
                    println!("Failed to send packet: {:?}", e);
                    sleep(Duration::from_secs(1));
                    next = Instant::now();
                    break;
                }
            }
        }
        // Sleep if needed (i.e. if we are quicker than the target Mb/s)
        if let Some(interval) = interval {
            next += interval;
            match next.checked_duration_since(Instant::now()) {
                None => {
                    next = Instant::now();
                }
                Some(ahead) => sleeper.sleep(ahead),
            }
        }
    }
}
