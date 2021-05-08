extern crate tokio_crate as tokio;

use anyhow::{anyhow, Context, Result};
use async_smoltcp::{device::FutureDevice, util::AsyncCapture, Net, NetConfig};
use futures::{FutureExt, SinkExt, StreamExt};
use pcap::{Capture, Device};
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, Ipv4Address, Ipv4Cidr};
use std::{future::ready, io};
use structopt::StructOpt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, StructOpt)]
struct Opt {
    device: String,
}

fn map_err(e: pcap::Error) -> io::Error {
    match e {
        pcap::Error::IoError(e) => e.into(),
        pcap::Error::TimeoutExpired => io::ErrorKind::WouldBlock.into(),
        other => io::Error::new(io::ErrorKind::Other, other),
    }
}

async fn async_main(opt: Opt) -> Result<()> {
    let device = Device::list()?
        .into_iter()
        .find(|d| d.name == opt.device)
        .ok_or(anyhow!("Device not found"))?;

    let cap = Capture::from_device(device)
        .context("Failed to capture device")?
        .promisc(true)
        .immediate_mode(true)
        .open()
        .context("Failed to open device")?;

    let async_cap = AsyncCapture::new(
        cap.setnonblock().context("Failed to set nonblock")?,
        |d| d.next().map_err(map_err).map(|p| p.to_vec()),
        |d, pkt| d.sendpacket(pkt).map_err(map_err),
    )
    .context("Failed to create async capture")?
    .take_while(|i| ready(i.is_ok()))
    .map(|i| i.unwrap());

    let device = FutureDevice::new(async_cap, 1500, |d| tokio::time::sleep(d).boxed());
    let (net, fut) = Net::new(
        device,
        NetConfig {
            ethernet_addr: EthernetAddress::from_bytes(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x05]),
            ip_addr: IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(172, 19, 44, 33), 20)),
            gateway: IpAddress::Ipv4(Ipv4Address::new(172, 19, 32, 1)),
        },
    );
    tokio::spawn(fut);

    let mut tcp = net.tcp_connect("39.156.69.79:80".parse()?).await?;
    println!("Connected");

    tcp.write_all(b"GET / HTTP/1.1\r\nHost: www.baidu.com\r\n\r\n")
        .await?;
    println!("Sent");

    let mut string = String::new();
    tcp.read_to_string(&mut string).await?;
    println!("read {}", string);

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();
    if let Err(e) = async_main(opt).await {
        eprintln!("Error {:?}", e);
    }
    Ok(())
}
