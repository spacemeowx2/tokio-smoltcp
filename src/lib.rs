//! An asynchronous wrapper for smoltcp.

use std::{
    collections::BTreeMap,
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
};

use device::BufferDevice;
use futures::Future;
use managed::ManagedMap;
use reactor::Reactor;
pub use smoltcp;
use smoltcp::{
    iface::{InterfaceBuilder, NeighborCache, Route, Routes},
    phy::Medium,
    time::{Duration, Instant},
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpProtocol, IpVersion},
};
pub use socket::{RawSocket, TcpListener, TcpStream, UdpSocket};
pub use socket_allocator::BufferSize;
use tokio::sync::Notify;

/// The async devices.
pub mod device;
mod reactor;
mod socket;
mod socket_allocator;

/// Can be used to create a forever timestamp in neighbor.
// The 60_000 is the same as NeighborCache::ENTRY_LIFETIME.
pub const FOREVER: Instant =
    Instant::from_micros_const(i64::max_value() - Duration::from_millis(60_000).micros() as i64);

pub struct Neighbor {
    pub protocol_addr: IpAddress,
    pub hardware_addr: HardwareAddress,
    pub timestamp: Instant,
}

/// A config for a `Net`.
///
/// This is used to configure the `Net`.
pub struct NetConfig {
    pub ethernet_addr: EthernetAddress,
    pub ip_addr: IpCidr,
    pub gateway: Vec<IpAddress>,
    pub buffer_size: BufferSize,
    pub neighbor_cache: Vec<Neighbor>,
}

/// `Net` is the main interface to the network stack.
/// Socket creation and configuration is done through the `Net` interface.
///
/// When `Net` is dropped, all sockets are closed and the network stack is stopped.
pub struct Net {
    reactor: Arc<Reactor>,
    ip_addr: IpCidr,
    from_port: AtomicU16,
    stopper: Arc<Notify>,
}

impl Net {
    /// Creates a new `Net` instance. It panics if the medium is not supported.
    pub fn new<D: device::AsyncDevice + 'static>(device: D, config: NetConfig) -> Net {
        let (net, fut) = Self::new2(device, config);
        tokio::spawn(fut);
        net
    }

    fn new2<D: device::AsyncDevice + 'static>(
        device: D,
        config: NetConfig,
    ) -> (Net, impl Future<Output = io::Result<()>> + Send) {
        let mut routes = Routes::new(BTreeMap::new());
        for gateway in config.gateway {
            match gateway {
                IpAddress::Ipv4(v4) => {
                    routes.add_default_ipv4_route(v4).unwrap();
                }
                IpAddress::Ipv6(v6) => {
                    routes.add_default_ipv6_route(v6).unwrap();
                }
                _ => panic!("Unsupported address"),
            };
        }
        let mut neighbor_cache = NeighborCache::new(BTreeMap::new());
        for n in config.neighbor_cache {
            neighbor_cache.fill(n.protocol_addr, n.hardware_addr, n.timestamp);
        }
        let buffer_device = BufferDevice::new(device.capabilities().clone());
        let interf = match device.capabilities().medium {
            Medium::Ethernet => InterfaceBuilder::new(buffer_device, vec![])
                .hardware_addr(config.ethernet_addr.into())
                .neighbor_cache(neighbor_cache)
                .ip_addrs(vec![config.ip_addr.clone()])
                .routes(routes)
                .finalize(),
            Medium::Ip => InterfaceBuilder::new(buffer_device, vec![])
                .ip_addrs(vec![config.ip_addr.clone()])
                .routes(routes)
                .finalize(),
            #[allow(unreachable_patterns)]
            _ => panic!("Unsupported medium"),
        };
        let stopper = Arc::new(Notify::new());
        let (reactor, fut) = Reactor::new(device, interf, config.buffer_size, stopper.clone());

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
    /// Creates a new TcpListener, which will be bound to the specified address.
    pub async fn tcp_bind(&self, addr: SocketAddr) -> io::Result<TcpListener> {
        let addr = self.set_address(addr);
        TcpListener::new(self.reactor.clone(), addr.into()).await
    }
    /// Opens a TCP connection to a remote host.
    pub async fn tcp_connect(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        TcpStream::connect(
            self.reactor.clone(),
            (self.ip_addr.address(), self.get_port()).into(),
            addr.into(),
        )
        .await
    }
    /// This function will create a new UDP socket and attempt to bind it to the `addr` provided.
    pub async fn udp_bind(&self, addr: SocketAddr) -> io::Result<UdpSocket> {
        let addr = self.set_address(addr);
        UdpSocket::new(self.reactor.clone(), addr.into()).await
    }
    /// Creates a new raw socket.
    pub async fn raw_socket(
        &self,
        ip_version: IpVersion,
        ip_protocol: IpProtocol,
    ) -> io::Result<RawSocket> {
        RawSocket::new(self.reactor.clone(), ip_version, ip_protocol).await
    }
    fn set_address(&self, mut addr: SocketAddr) -> SocketAddr {
        if addr.ip().is_unspecified() {
            addr.set_ip(match self.ip_addr.address() {
                IpAddress::Ipv4(ip) => Ipv4Addr::from(ip).into(),
                IpAddress::Ipv6(ip) => Ipv6Addr::from(ip).into(),
                _ => panic!("address must not be unspecified"),
            });
        }
        if addr.port() == 0 {
            addr.set_port(self.get_port());
        }
        addr
    }

    /// Updates the routes of the network stack.    
    pub fn update_routes<F: FnOnce(&mut ManagedMap<'static, IpCidr, Route>)>(&self, f: F) {
        let interf = self.reactor.interf().clone();
        let mut interf = interf.lock();
        interf.routes_mut().update(f);
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        self.stopper.notify_waiters()
    }
}
