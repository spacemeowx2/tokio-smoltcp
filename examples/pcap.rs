use anyhow::{anyhow, Context, Result};
use dns_parser::QueryType;
use pcap::{Capture, Device};
use smoltcp::{
    phy::DeviceCapabilities,
    wire::{EthernetAddress, IpAddress, IpCidr},
};
use structopt::StructOpt;
use tokio::io::{copy, split, AsyncReadExt, AsyncWriteExt};
use tokio_smoltcp::{device::AsyncDevice, Net, NetConfig};

#[derive(Debug, StructOpt)]
struct Opt {
    device: String,
    #[structopt(short, long, default_value = "00:01:02:03:04:05")]
    ethernet_addr: String,
    #[structopt(short, long, default_value = "172.19.44.33/20")]
    ip_addr: String,
    #[structopt(short, long, default_value = "172.19.32.1")]
    gateway: String,
}

#[cfg(unix)]
fn get_by_device(device: Device) -> Result<impl AsyncDevice> {
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
    let mut caps = DeviceCapabilities::default();
    caps.max_burst_size = Some(100);
    caps.max_transmission_unit = 1500;

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
        caps,
    )
    .context("Failed to create async capture")?)
}

#[cfg(windows)]
fn get_by_device(device: Device) -> Result<impl AsyncDevice> {
    use tokio::sync::mpsc::{Receiver, Sender};
    use tokio_smoltcp::util::ChannelCapture;
    let mut caps = DeviceCapabilities::default();
    caps.max_burst_size = Some(100);
    caps.max_transmission_unit = 1500;

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

    let recv = move |tx: Sender<std::io::Result<Vec<u8>>>| loop {
        let p = match cap.next().map(|p| p.to_vec()) {
            Ok(p) => p,
            Err(pcap::Error::TimeoutExpired) => continue,
            Err(e) => {
                eprintln!("Error: {:?}", e);
                break;
            }
        };
        tx.blocking_send(Ok(p)).unwrap();
    };
    let send = move |mut rx: Receiver<Vec<u8>>| {
        while let Some(pkt) = rx.blocking_recv() {
            send.sendpacket(pkt).unwrap();
        }
    };
    let capture = ChannelCapture::new(recv, send, caps);
    Ok(capture)
}

async fn async_main(opt: Opt) -> Result<()> {
    let device = Device::list()?
        .into_iter()
        .find(|d| d.name == opt.device)
        .ok_or(anyhow!("Device not found"))?;
    let ethernet_addr: EthernetAddress = opt.ethernet_addr.parse().unwrap();
    let ip_addr: IpCidr = opt.ip_addr.parse().unwrap();
    let gateway: IpAddress = opt.gateway.parse().unwrap();

    let device = get_by_device(device)?;
    let (net, fut) = Net::new(
        device,
        NetConfig {
            ethernet_addr,
            ip_addr,
            gateway: vec![gateway],
            buffer_size: Default::default(),
        },
    );
    tokio::spawn(fut);

    let udp = net.udp_bind("0.0.0.0:0".parse()?).await?;
    println!("udp local_addr {:?}", udp.local_addr());
    let mut query_builder = dns_parser::Builder::new_query(1, true);
    query_builder.add_question(
        "www.baidu.com",
        false,
        QueryType::A,
        dns_parser::QueryClass::IN,
    );
    let query = query_builder.build().unwrap();
    udp.send_to(&query, "114.114.114.114:53".parse()?).await?;
    let mut answer = [0u8; 1024];
    let (size, _) = udp.recv_from(&mut answer).await?;
    let packet = dns_parser::Packet::parse(&answer[..size])?;
    println!("dns answer packet: {:#?}", packet);
    let dst_ip = packet
        .answers
        .iter()
        .filter_map(|a| match a.data {
            dns_parser::RData::A(dns_parser::rdata::A(ip)) => Some(ip),
            _ => None,
        })
        .next()
        .expect("No A record in response");

    println!("Connecting www.baidu.com");
    let mut tcp = net.tcp_connect((dst_ip, 80).into()).await?;
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
