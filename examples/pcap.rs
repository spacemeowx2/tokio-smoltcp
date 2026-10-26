use anyhow::{Context, Result, anyhow};
use dns_parser::QueryType;
use pcap::Device;
use smoltcp::{
    phy::DeviceCapabilities,
    wire::{EthernetAddress, IpAddress, IpCidr},
};
use structopt::StructOpt;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy, split};
use tokio_smoltcp::{Net, NetConfig, TcpConnectOptions, device::AsyncDevice, smoltcp::iface};

#[derive(Debug, StructOpt)]
struct Opt {
    device: String,
    /// Verify DNS, HTTP, flush and shutdown, then exit within 30 seconds.
    #[structopt(long)]
    smoke: bool,
    #[structopt(short, long, default_value = "00:01:02:03:04:05")]
    ethernet_addr: String,
    #[structopt(short, long, default_value = "172.19.44.33/20")]
    ip_addr: String,
    #[structopt(short, long, default_value = "172.19.32.1")]
    gateway: String,
}

#[cfg(unix)]
fn get_by_device(device: Device) -> Result<impl AsyncDevice> {
    use pcap::{Active, Capture};
    use std::{
        ffi::c_void,
        io,
        os::fd::{AsFd, BorrowedFd, RawFd},
    };
    use tokio::io::Interest;
    use tokio_smoltcp::device::AsyncCapture;

    #[link(name = "pcap")]
    unsafe extern "C" {
        fn pcap_get_selectable_fd(handle: *mut c_void) -> i32;
    }

    struct SelectableCapture {
        cap: Capture<Active>,
        fd: RawFd,
    }

    impl AsFd for SelectableCapture {
        fn as_fd(&self) -> BorrowedFd<'_> {
            // SAFETY: cap owns the validated selectable descriptor for this borrow.
            unsafe { BorrowedFd::borrow_raw(self.fd) }
        }
    }

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
            other => io::Error::other(other),
        }
    }

    let cap = cap.setnonblock().context("Failed to set nonblock")?;
    let fd = unsafe { pcap_get_selectable_fd(cap.as_ptr().cast::<c_void>()) };
    if fd < 0 {
        return Err(anyhow!("pcap device does not expose a selectable fd"));
    }
    let cap = SelectableCapture { cap, fd };

    let mut caps = DeviceCapabilities::default();
    caps.max_burst_size = Some(100);
    caps.max_transmission_unit = 1500;

    AsyncCapture::new_with_interest(
        cap,
        |d| {
            let r = d.cap.next_packet().map_err(map_err).map(|p| p.to_vec());
            // eprintln!("recv {:?}", r);
            r
        },
        |d, pkt| {
            let r = d.cap.sendpacket(pkt).map_err(map_err);
            // eprintln!("send {:?}", r);
            r
        },
        caps,
        Interest::READABLE,
    )
    .context("Failed to create async capture with selectable fd")
}

#[cfg(windows)]
fn get_by_device(device: Device) -> Result<impl AsyncDevice> {
    use tokio::sync::mpsc::{Receiver, Sender};
    use tokio_smoltcp::device::ChannelCapture;
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
        let p = match cap.next_packet().map(|p| p.to_vec()) {
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
    let mut interface_config = iface::Config::new(ethernet_addr.into());
    interface_config.random_seed = rand::random();
    let net = Net::new(
        device,
        NetConfig::new(interface_config, ip_addr, vec![gateway]),
    );

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
    let options = TcpConnectOptions::default().with_source_port(20000);
    let mut tcp = net
        .tcp_connect_with_options((dst_ip, 80).into(), options)
        .await?;
    println!("Connected");

    tcp.write_all(b"GET / HTTP/1.0\r\nHost: www.baidu.com\r\n\r\n")
        .await?;
    tcp.flush().await?;
    println!("Sent");

    let mut string = String::new();
    tcp.read_to_string(&mut string).await?;
    if opt.smoke {
        anyhow::ensure!(
            string.starts_with("HTTP/1.0 200 ") || string.starts_with("HTTP/1.1 200 "),
            "expected a successful HTTP response"
        );
        println!(
            "HTTP response: {} bytes, {}",
            string.len(),
            string.lines().next().unwrap()
        );
        tcp.shutdown().await?;
        println!("PASS: DNS, TCP source port 20000, HTTP 200, flush, EOF and shutdown");
        return Ok(());
    }
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
    if opt.smoke {
        tokio::time::timeout(std::time::Duration::from_secs(30), async_main(opt))
            .await
            .context("real-interface smoke timed out")?
    } else {
        async_main(opt).await
    }
}
