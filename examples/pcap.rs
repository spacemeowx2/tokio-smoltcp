extern crate tokio_crate as tokio;

use anyhow::{anyhow, Result};
use async_smoltcp::{device::FutureDevice, util::AsyncCapture, Net};
use futures::{future::BoxFuture, FutureExt, SinkExt, StreamExt};
use pcap::{Capture, Device};
use std::{future::ready, io, time::Duration};
use structopt::StructOpt;

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

    let cap = Capture::from_device(device)?
        .promisc(true)
        .immediate_mode(true)
        .open()?;

    let async_cap = AsyncCapture::new(
        cap.setnonblock()?,
        |d| d.next().map_err(map_err).map(|p| p.to_vec()),
        |d, pkt| d.sendpacket(pkt).map_err(map_err),
    )?
    .take_while(|i| ready(i.is_ok()))
    .map(|i| i.unwrap());

    let device = FutureDevice::new(async_cap, 1500, |d| tokio::time::sleep(d).boxed());
    let (net, fut) = Net::new(device);
    tokio::spawn(fut);

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();
    async_main(opt).await
}
