use anyhow::{anyhow, Context, Result};
use pcap::{Capture, Device};
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, Ipv4Address, Ipv4Cidr};
use structopt::StructOpt;
use tokio::io::{copy, split, AsyncReadExt, AsyncWriteExt};
use tokio_smoltcp::{
    device::{FutureDevice, Interface},
    Net, NetConfig,
};

#[derive(Debug, StructOpt)]
struct Opt {
    device: String,
}

#[cfg(unix)]
fn get_by_device(device: Device) -> Result<impl Interface> {
    use futures::StreamExt;
    use std::future::ready;
    use std::io;
    use tokio_smoltcp::util::AsyncCapture;

    let cap = Capture::from_device(device.clone())
        .context("Failed to capture device")?
        .promisc(true)
        .immediate_mode(true)
        .timeout(5)
        .open()
        .context("Failed to open device")?;

    fn map_err(e: pcap::Error) -> io::Error {
        match e {
            pcap::Error::IoError(e) => e.into(),
            pcap::Error::TimeoutExpired => io::ErrorKind::WouldBlock.into(),
            other => io::Error::new(io::ErrorKind::Other, other),
        }
    }

    Ok(AsyncCapture::new(
        cap.setnonblock().context("Failed to set nonblock")?,
        |d| {
            let r = d.next().map_err(map_err).map(|p| p.to_vec());
            // eprintln!("recv {:?}", r);
            r
        },
        |d, pkt| {
            let r = d.sendpacket(pkt).map_err(map_err);
            // eprintln!("send {:?}", r);
            r
        },
    )
    .context("Failed to create async capture")?
    .take_while(|i| ready(i.is_ok()))
    .map(|i| i.unwrap()))
}

#[cfg(windows)]
fn get_by_device(device: Device) -> Result<impl Interface> {
    use tokio::sync::mpsc::{Receiver, Sender};
    use tokio_smoltcp::util::ChannelCapture;

    let mut cap = Capture::from_device(device.clone())
        .context("Failed to capture device")?
        .promisc(true)
        .immediate_mode(true)
        .timeout(5)
        .open()
        .context("Failed to open device")?;
    let mut send = Capture::from_device(device)
        .context("Failed to capture device")?
        .promisc(true)
        .immediate_mode(true)
        .timeout(5)
        .open()
        .context("Failed to open device")?;

    let recv = move |tx: Sender<Vec<u8>>| loop {
        let p = match cap.next().map(|p| p.to_vec()) {
            Ok(p) => p,
            Err(pcap::Error::TimeoutExpired) => continue,
            Err(e) => {
                eprintln!("Error: {:?}", e);
                break;
            }
        };
        tx.blocking_send(p).unwrap();
    };
    let send = move |mut rx: Receiver<Vec<u8>>| {
        while let Some(pkt) = rx.blocking_recv() {
            send.sendpacket(pkt).unwrap();
        }
    };
    let capture = ChannelCapture::new(recv, send);
    Ok(capture)
}

async fn async_main(opt: Opt) -> Result<()> {
    let device = Device::list()?
        .into_iter()
        .find(|d| d.name == opt.device)
        .ok_or(anyhow!("Device not found"))?;

    let device = FutureDevice::new(get_by_device(device)?, 1500);
    let (net, fut) = Net::new(
        device,
        NetConfig {
            ethernet_addr: EthernetAddress::from_bytes(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x05]),
            ip_addr: IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(172, 19, 44, 33), 20)),
            gateway: IpAddress::Ipv4(Ipv4Address::new(172, 19, 32, 1)),
            buffer_size: Default::default(),
        },
    );
    tokio::spawn(fut);

    println!("Connecting");
    let mut tcp = net.tcp_connect("39.156.69.79:80".parse()?).await?;
    println!("Connected");

    tcp.write_all(b"GET / HTTP/1.0\r\nHost: www.baidu.com\r\n\r\n")
        .await?;
    println!("Sent");

    let mut string = String::new();
    tcp.read_to_string(&mut string).await?;
    println!("read {}", string);

    let mut listener = net.tcp_bind("0.0.0.0:12345".parse()?).await?;
    loop {
        let (tcp, addr) = listener.accept().await?;
        println!("Accept from {:?}", addr);
        tokio::spawn(async move {
            let (mut tx, mut rx) = split(tcp);
            copy(&mut tx, &mut rx).await.unwrap();
        });
    }

    // Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();
    if let Err(e) = async_main(opt).await {
        eprintln!("Error {:?}", e);
    }
    Ok(())
}
