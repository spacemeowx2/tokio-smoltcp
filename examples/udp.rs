// This example contains implementation for two different endpoints connected
// over virtual network organized with smoltcp.
//
// Endpoints (think of them like a computers in LAN) will send ethernet
// frames to each other over UDP (UDP is like a wire in your virtual network).
//
// You can start server with:
// cargo run --example udp -- --server
//
// and client with:
// RUST_LOG=trace cargo run --example udp
//
// Client will send UDP pings to the server with timeout and server will send
// pongs back to the client. You can stop server and start it back and see
// that client will not recieve pong while server is not available.
//
// Advanced level
// You can use socat instead of client or server. Example for client on linux:
// socat TUN,tun-type=tap,iff-up,iff-no-pi UDP4:127.0.0.1:12345,bind=127.0.0.1:12346
// ifconfig tap<X> up && ip addr add dev tap<X> 172.19.44.34/20 && ping 172.19.44.33
// socat UDP4:172.19.44.33:8000 -

use log::{debug, error};
use smoltcp::wire::{EthernetAddress, IpCidr};
use std::{net::SocketAddr, time::Duration};
use structopt::StructOpt;
use tokio_smoltcp::{join::udp_device, Net, NetConfig};
mod signals;

#[derive(Debug, StructOpt)]
struct Opt {
    #[structopt(short, long)]
    server: bool,
}

async fn client_task(net: Net, server_address: SocketAddr) -> anyhow::Result<()> {
    let udp = net.udp_bind("0.0.0.0:0".parse()?).await?;
    let mut packet_counter = 0;
    let mut answer = [0u8; 1024];
    loop {
        udp.send_to(b"12345", server_address).await?;
        debug!("sent packet {} to {:?}", packet_counter, server_address);
        tokio::select!(
            Ok((size, addr)) = udp.recv_from(&mut answer) => {
                debug!("received {:?}, from {:?}", &answer[..size], addr);
            },
            _ = tokio::time::sleep(Duration::from_millis(1000)) => {
                error!("response {} timed out", packet_counter);
            },
        );
        packet_counter += 1;
    }
}

async fn server_task(net: Net) -> anyhow::Result<()> {
    let udp = net.udp_bind("0.0.0.0:8000".parse()?).await?;
    let mut answer = [0u8; 1024];
    loop {
        let (size, addr) = udp.recv_from(&mut answer).await?;
        debug!("received {:?}, from {:?}", &answer[..size], addr);
        udp.send_to(b"12345", addr).await?;
        debug!("sent packet back to {:?}", addr);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let opt = Opt::from_args();

    let server_address: String = "172.19.44.33".to_string();

    // network-over-udp addresses
    let (ethernet_addr, ip_addr) = if opt.server {
        ("00:01:02:03:04:05", format!("{}/20", server_address))
    } else {
        ("00:01:02:03:04:06", "172.19.44.34/20".to_string())
    };
    let ethernet_addr: EthernetAddress = ethernet_addr.parse().unwrap();
    let ip_addr: IpCidr = ip_addr.parse().unwrap();

    // outside network communication addresses
    let (local_addr, remote_addr) = if opt.server {
        ("127.0.0.1:12345", "127.0.0.1:12346")
    } else {
        ("127.0.0.1:12346", "127.0.0.1:12345")
    };

    let (net, fut) = Net::new(
        udp_device(local_addr.parse().unwrap(), remote_addr.parse().unwrap()).await?,
        NetConfig {
            ethernet_addr,
            ip_addr,
            gateway: Vec::new(),
            buffer_size: Default::default(),
        },
    );
    tokio::spawn(fut);
    if opt.server {
        tokio::spawn(server_task(net));
    } else {
        tokio::spawn(client_task(
            net,
            format!("{}:8000", server_address).parse().unwrap(),
        ));
    };

    Ok(signals::shutdown_signal().await)
}
