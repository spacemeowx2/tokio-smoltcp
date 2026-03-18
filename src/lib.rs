//! An asynchronous wrapper for smoltcp.

use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
};

use device::BufferDevice;
use futures::Future;
use reactor::Reactor;
pub use smoltcp;
use smoltcp::{
    iface::{Config, Interface, Routes},
    time::{Duration, Instant},
    wire::{HardwareAddress, IpAddress, IpCidr, IpListenEndpoint, IpProtocol, IpVersion},
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
#[non_exhaustive]
pub struct NetConfig {
    pub interface_config: Config,
    pub ip_addr: IpCidr,
    pub gateway: Vec<IpAddress>,
    pub buffer_size: BufferSize,
}

impl NetConfig {
    pub fn new(interface_config: Config, ip_addr: IpCidr, gateway: Vec<IpAddress>) -> Self {
        Self {
            interface_config,
            ip_addr,
            gateway,
            buffer_size: Default::default(),
        }
    }
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
        let mut buffer_device = BufferDevice::new(device.capabilities().clone());
        let mut iface = Interface::new(config.interface_config, &mut buffer_device, Instant::now());
        let ip_addr = config.ip_addr;
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs.push(ip_addr).unwrap();
        });
        for gateway in config.gateway {
            match gateway {
                IpAddress::Ipv4(v4) => {
                    iface.routes_mut().add_default_ipv4_route(v4).unwrap();
                }
                IpAddress::Ipv6(v6) => {
                    iface.routes_mut().add_default_ipv6_route(v6).unwrap();
                }
                #[allow(unreachable_patterns)]
                _ => panic!("Unsupported address"),
            };
        }

        let stopper = Arc::new(Notify::new());
        let (reactor, fut) = Reactor::new(
            device,
            iface,
            buffer_device,
            config.buffer_size,
            stopper.clone(),
        );

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
        let (addr, endpoint) = self.bind_address(addr);
        TcpListener::new(self.reactor.clone(), endpoint, addr).await
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
    pub fn tcp_connect_lazy(
        &self,
        addr: SocketAddr,
    ) -> (
        SocketAddr,
        impl Future<Output = Result<TcpStream, std::io::Error>>,
    ) {
        let local_endpoint: SocketAddr = (self.ip_addr.address(), self.get_port()).into();
        let future = TcpStream::connect(self.reactor.clone(), local_endpoint.into(), addr.into());
        (local_endpoint, future)
    }
    /// This function will create a new UDP socket and attempt to bind it to the `addr` provided.
    pub async fn udp_bind(&self, addr: SocketAddr) -> io::Result<UdpSocket> {
        let (addr, endpoint) = self.bind_address(addr);
        UdpSocket::new(self.reactor.clone(), endpoint, addr).await
    }
    /// Creates a new raw socket.
    pub async fn raw_socket(
        &self,
        ip_version: IpVersion,
        ip_protocol: IpProtocol,
    ) -> io::Result<RawSocket> {
        RawSocket::new(self.reactor.clone(), ip_version, ip_protocol).await
    }
    fn bind_address(&self, mut addr: SocketAddr) -> (SocketAddr, IpListenEndpoint) {
        if addr.port() == 0 {
            addr.set_port(self.get_port());
        }

        let endpoint = if addr.ip().is_unspecified() {
            addr.port().into()
        } else {
            addr.into()
        };

        (addr, endpoint)
    }

    /// Enable or disable the AnyIP capability.
    pub fn set_any_ip(&self, any_ip: bool) {
        self.reactor.with_iface(|iface| iface.set_any_ip(any_ip));
    }

    /// Get whether AnyIP is enabled.
    pub fn any_ip(&self) -> bool {
        self.reactor.with_iface(|iface| iface.any_ip())
    }

    pub fn routes<F: FnOnce(&Routes)>(&self, f: F) {
        self.reactor.with_iface(|iface| f(iface.routes()));
    }

    pub fn routes_mut<F: FnOnce(&mut Routes)>(&self, f: F) {
        self.reactor.with_iface(|iface| f(iface.routes_mut()));
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        self.stopper.notify_waiters()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{Sink, SinkExt, Stream};
    use smoltcp::phy::{DeviceCapabilities, Medium};
    use std::{
        io,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
        time::Duration as StdDuration,
    };

    #[derive(Clone)]
    struct PendingDevice {
        caps: DeviceCapabilities,
    }

    impl Stream for PendingDevice {
        type Item = io::Result<device::Packet>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Sink<device::Packet> for PendingDevice {
        type Error = io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _item: device::Packet) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl device::AsyncDevice for PendingDevice {
        fn capabilities(&self) -> &DeviceCapabilities {
            &self.caps
        }
    }

    struct EofDevice {
        caps: DeviceCapabilities,
    }

    impl Stream for EofDevice {
        type Item = io::Result<device::Packet>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(None)
        }
    }

    impl Sink<device::Packet> for EofDevice {
        type Error = io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _item: device::Packet) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl device::AsyncDevice for EofDevice {
        fn capabilities(&self) -> &DeviceCapabilities {
            &self.caps
        }
    }

    fn ip_caps() -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = 1500;
        caps.max_burst_size = Some(1);
        caps
    }

    fn test_config() -> NetConfig {
        let mut interface_config = Config::new(HardwareAddress::Ip);
        interface_config.random_seed = 1;
        NetConfig::new(
            interface_config,
            IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24),
            vec![],
        )
    }

    #[tokio::test]
    async fn tcp_bind_keeps_unspecified_addr_for_wildcard_bind() {
        let (net, fut) = Net::new2(PendingDevice { caps: ip_caps() }, test_config());
        tokio::spawn(fut);

        let listener = net.tcp_bind("0.0.0.0:12345".parse().unwrap()).await.unwrap();

        assert!(
            listener.local_addr().unwrap().ip().is_unspecified(),
            "wildcard tcp bind should preserve an unspecified local address",
        );
    }

    #[tokio::test]
    async fn udp_bind_keeps_unspecified_addr_for_wildcard_bind() {
        let (net, fut) = Net::new2(PendingDevice { caps: ip_caps() }, test_config());
        tokio::spawn(fut);

        let socket = net.udp_bind("0.0.0.0:12345".parse().unwrap()).await.unwrap();

        assert!(
            socket.local_addr().unwrap().ip().is_unspecified(),
            "wildcard udp bind should preserve an unspecified local address",
        );
    }

    #[test]
    fn any_ip_round_trips_through_public_api() {
        let (net, _fut) = Net::new2(PendingDevice { caps: ip_caps() }, test_config());

        assert!(!net.any_ip());
        net.set_any_ip(true);
        assert!(net.any_ip());
        net.set_any_ip(false);
        assert!(!net.any_ip());
    }

    #[tokio::test]
    async fn bind_with_port_zero_allocates_an_ephemeral_port() {
        let (net, fut) = Net::new2(PendingDevice { caps: ip_caps() }, test_config());
        tokio::spawn(fut);

        let tcp = net.tcp_bind("0.0.0.0:0".parse().unwrap()).await.unwrap();
        let udp = net.udp_bind("0.0.0.0:0".parse().unwrap()).await.unwrap();

        assert_ne!(tcp.local_addr().unwrap().port(), 0);
        assert_ne!(udp.local_addr().unwrap().port(), 0);
    }

    #[tokio::test]
    async fn tcp_connect_returns_error_when_socket_closes_during_handshake() {
        let (net, fut) = Net::new2(PendingDevice { caps: ip_caps() }, test_config());
        tokio::spawn(fut);
        let mut connect = Box::pin(net.tcp_connect("10.0.0.2:80".parse().unwrap()));

        assert!(matches!(futures::poll!(&mut connect), Poll::Pending));

        assert!(
            net.reactor.close_first_tcp_socket_for_test().await,
            "test setup should create exactly one tcp socket",
        );

        let result = tokio::time::timeout(StdDuration::from_millis(50), &mut connect).await;
        let connect_result = result.expect("connect future should resolve after the socket closes");
        assert!(
            connect_result.is_err(),
            "connect should fail once the in-flight socket is closed",
        );
    }

    #[tokio::test]
    async fn reactor_stops_when_device_stream_ends() {
        let (_net, fut) = Net::new2(EofDevice { caps: ip_caps() }, test_config());

        let result = tokio::time::timeout(StdDuration::from_millis(50), fut).await;

        assert!(
            matches!(result, Ok(Ok(()))),
            "reactor should terminate cleanly when the device stream ends, got {result:?}",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn async_capture_retries_buffered_packet_on_flush() {
        use crate::device::AsyncCapture;
        use std::os::unix::net::UnixStream;

        let (stream, _peer) = UnixStream::pair().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let send_attempts = attempts.clone();
        let mut capture = AsyncCapture::new(
            stream,
            |_obj| Err(io::ErrorKind::WouldBlock.into()),
            move |_obj, _pkt| {
                let attempt = send_attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Err(io::ErrorKind::WouldBlock.into())
                } else {
                    Ok(())
                }
            },
            ip_caps(),
        )
        .unwrap();

        capture.send(vec![1, 2, 3]).await.unwrap();

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "flush should retry the buffered packet after the initial WouldBlock",
        );
    }
}
