# tokio-smoltcp

A async wrapper for [`smoltcp`](https://github.com/smoltcp-rs/smoltcp).

## Note

`v0.1.x` does not follow semver, the API may change at any time.

## Example

See [examples/pcap.rs](examples/pcap.rs).

```sh
cargo build --example pcap && sudo ./target/debug/examples/pcap -h
```

This example uses [`pcap`](https://crates.io/crates/pcap) as backend and do the following things:

1. Bind a UDP port and send a DNS request to `114.114.114.114:53` and get the `www.baidu.com`'s IP.
2. Create a `TcpStream` and send a simple HTTP/1.0 request, then print the HTTP response.
3. Create a echo server listening to `0.0.0.0:12345`, accept incoming connections and send the data back.
