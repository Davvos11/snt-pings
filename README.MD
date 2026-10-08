# SNT Jinglepings ping toolie

## Usage:
```shell
cargo run --release -- filename.png x y --timeout microsecs
```
Or:
```shell
cargo build --release
./target/release/snt-pings filename.png x y --timeout microsecs
```

Might need to grant permissions to open ICMP sockets (resets after a reboot):
```shell
sudo sysctl -w net.ipv4.ping_group_range="$(id -g) $(id -g)"
```