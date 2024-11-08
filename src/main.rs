use std::net::{Ipv6Addr, SocketAddr};
use std::thread::sleep;
use std::time::Duration;
use clap::Parser;
use rand::Rng;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    filename: String,
    x: usize,
    y: usize,
    #[arg(short, long, default_value = "5")]
    timeout: u64,
}

fn main() {
    let args = Args::parse();

    let img = image::open(args.filename).expect("Failed to open image");
    let rgba_data = img.to_rgba8().into_raw();
    let width = img.width() as usize;

    let addresses: Vec<_> = rgba_data
        .chunks(4)
        .enumerate()
        .filter(|(_, rgba)| rgba[3] > 0)
        .map(|(i, rgba)| {
            let x = args.x + (i % width);
            let y = args.y + (i / width);
            let addr = Ipv6Addr::new(0x2001, 0x610, 0x1908, 0xa000,
                                     x as u16, y as u16,
                                     (rgba[2] as u16) << 8 | rgba[1] as u16,
                                     (rgba[0] as u16) << 8 | rgba[3] as u16,
            );
            SocketAddr::new(addr.into(), 0)
        })
        .collect();

    dbg!(addresses.len());

    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::ICMPV6))
        .expect("Could not open socket");

    // Create an ICMPv6 Echo Request packet
    let mut packet = [0u8; 8];
    packet[0] = 128; // Type: 128 (Echo Request)
    packet[1] = 0;   // Code: 0 (no special code)
    packet[2] = 0;   // Checksum: 0 for now, can be calculated later if needed
    packet[3] = 0;   // Checksum (high byte)
    packet[4] = 0;   // Identifier (low byte, arbitrary)
    packet[5] = 1;   // Identifier (high byte, arbitrary)
    packet[6] = 0;   // Sequence Number (low byte)
    packet[7] = 1;   // Sequence Number (high byte)

    loop {
        let random_index = rand::thread_rng().gen_range(0..addresses.len());
        let address = addresses[random_index];

        socket.send_to(&packet, &SockAddr::from(address)).expect("Failed to send packet");
        if args.timeout > 0 {
            sleep(Duration::from_millis(args.timeout));
        }
    }
}
