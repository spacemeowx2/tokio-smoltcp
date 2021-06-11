use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
};

use device::FutureDevice;
use futures::Future;
use reactor::Reactor;
use smoltcp::{
    iface::{EthernetInterfaceBuilder, NeighborCache, Routes},
    wire::{EthernetAddress, IpAddress, IpCidr},
};
pub use socket::{TcpListener, TcpSocket, UdpSocket};
pub use socket_alloctor::BufferSize;
use tokio::sync::Notify;

pub mod device;
mod reactor;
mod socket;
mod socket_alloctor;
pub mod util;

pub struct NetConfig {
    pub ethernet_addr: EthernetAddress,
    pub ip_addr: IpCidr,
    pub gateway: IpAddress,
    pub buffer_size: BufferSize,
}

pub struct Net {
    reactor: Arc<Reactor>,
    ip_addr: IpCidr,
    from_port: AtomicU16,
    stopper: Arc<Notify>,
}

impl Net {
    pub fn new<S: device::Interface + 'static>(
        device: FutureDevice<S>,
        config: NetConfig,
    ) -> (Net, impl Future<Output = ()> + Send) {
        let mut routes = Routes::new(BTreeMap::new());
        match config.gateway {
            IpAddress::Ipv4(v4) => routes.add_default_ipv4_route(v4).unwrap(),
            _ => panic!("gateway should be set"),
        };
        let neighbor_cache = NeighborCache::new(BTreeMap::new());
        let interf = EthernetInterfaceBuilder::new(device)
            .ethernet_addr(config.ethernet_addr)
            .neighbor_cache(neighbor_cache)
            .ip_addrs(vec![config.ip_addr.clone()])
            .routes(routes)
            .finalize();
        let stopper = Arc::new(Notify::new());
        let (reactor, fut) = Reactor::new(interf, config.buffer_size, stopper.clone());

        (
            Net {
                reactor: Arc::new(reactor),
                ip_addr: config.ip_addr,
                from_port: AtomicU16::new(10001),
                stopper,
            },
            fut,
        )
    }
    fn get_port(&self) -> u16 {
        self.from_port
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |x| {
                Some(if x > 60000 { 10000 } else { x + 1 })
            })
            .unwrap()
    }
    pub async fn tcp_bind(&self, addr: SocketAddr) -> io::Result<TcpListener> {
        TcpListener::new(self.reactor.clone(), addr.into()).await
    }
    pub async fn tcp_connect(&self, addr: SocketAddr) -> io::Result<TcpSocket> {
        TcpSocket::connect(
            self.reactor.clone(),
            (self.ip_addr.address(), self.get_port()).into(),
            addr.into(),
        )
        .await
    }
    pub async fn udp_bind(&self, addr: SocketAddr) -> io::Result<UdpSocket> {
        UdpSocket::new(self.reactor.clone(), addr.into()).await
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        self.stopper.notify_waiters()
    }
}
