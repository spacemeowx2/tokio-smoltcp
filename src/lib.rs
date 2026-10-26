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
    Instant::from_micros_const(i64::MAX - Duration::from_millis(60_000).micros() as i64);

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

/// A config for creating a `TcpStream`
///
/// This is used to configure the `TcpStream`
#[derive(Default)]
pub struct TcpConnectOptions {
    source_port: Option<u16>,
}

impl TcpConnectOptions {
    pub fn with_source_port(mut self, source_port: u16) -> TcpConnectOptions {
        self.source_port = Some(source_port);
        self
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
            .update(Ordering::SeqCst, Ordering::SeqCst, |x| {
                if x > 60000 { 10000 } else { x + 1 }
            })
    }

    /// Creates a new TcpListener, which will be bound to the specified address.
    pub async fn tcp_bind(&self, addr: SocketAddr) -> io::Result<TcpListener> {
        let (addr, endpoint) = self.bind_address(addr);
        TcpListener::new(self.reactor.clone(), endpoint, addr).await
    }

    /// Opens a TCP connection to a remote host.
    pub async fn tcp_connect(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        self.tcp_connect_with_options(addr, TcpConnectOptions::default())
            .await
    }

    pub async fn tcp_connect_with_options(
        &self,
        addr: SocketAddr,
        options: TcpConnectOptions,
    ) -> io::Result<TcpStream> {
        let port = options.source_port.unwrap_or(self.get_port());
        TcpStream::connect(
            self.reactor.clone(),
            (self.ip_addr.address(), port).into(),
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
        self.tcp_connect_lazy_with_options(addr, TcpConnectOptions::default())
    }

    pub fn tcp_connect_lazy_with_options(
        &self,
        addr: SocketAddr,
        options: TcpConnectOptions,
    ) -> (
        SocketAddr,
        impl Future<Output = Result<TcpStream, std::io::Error>>,
    ) {
        let port = options.source_port.unwrap_or(self.get_port());
        let local_endpoint: SocketAddr = (self.ip_addr.address(), port).into();
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
        let iface = self.reactor.iface().clone();
        let mut iface: parking_lot::lock_api::MutexGuard<'_, parking_lot::RawMutex, Interface> =
            iface.lock();
        iface.set_any_ip(any_ip);
    }

    /// Get whether AnyIP is enabled.
    pub fn any_ip(&self) -> bool {
        let iface = self.reactor.iface().clone();
        let iface = iface.lock();
        iface.any_ip()
    }

    pub fn routes<F: FnOnce(&Routes)>(&self, f: F) {
        let iface = self.reactor.iface().clone();
        let iface = iface.lock();
        let routes = iface.routes();
        f(routes)
    }

    pub fn routes_mut<F: FnOnce(&mut Routes)>(&self, f: F) {
        let iface = self.reactor.iface().clone();
        let mut iface = iface.lock();
        let routes = iface.routes_mut();
        f(routes)
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        self.stopper.notify_one()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{Sink, SinkExt, Stream};
    use smoltcp::{
        phy::{DeviceCapabilities, Medium},
        socket::Socket,
    };
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
        let (net, _fut) = Net::new2(PendingDevice { caps: ip_caps() }, test_config());

        let listener = net
            .tcp_bind("0.0.0.0:12345".parse().unwrap())
            .await
            .unwrap();

        assert!(
            listener.local_addr().unwrap().ip().is_unspecified(),
            "wildcard tcp bind should preserve an unspecified local address",
        );
    }

    #[tokio::test]
    async fn udp_bind_keeps_unspecified_addr_for_wildcard_bind() {
        let (net, _fut) = Net::new2(PendingDevice { caps: ip_caps() }, test_config());

        let socket = net
            .udp_bind("0.0.0.0:12345".parse().unwrap())
            .await
            .unwrap();

        assert!(
            socket.local_addr().unwrap().ip().is_unspecified(),
            "wildcard udp bind should preserve an unspecified local address",
        );
    }

    #[tokio::test]
    async fn tcp_connect_returns_error_when_socket_closes_during_handshake() {
        let (net, _fut) = Net::new2(PendingDevice { caps: ip_caps() }, test_config());
        let mut connect = Box::pin(net.tcp_connect("10.0.0.2:80".parse().unwrap()));

        assert!(matches!(futures::poll!(&mut connect), Poll::Pending));

        {
            let mut sockets = net.reactor.socket_allocator().sockets().lock();
            let mut closed = false;
            for (_, socket) in sockets.iter_mut() {
                if let Socket::Tcp(tcp) = socket {
                    tcp.close();
                    closed = true;
                    break;
                }
            }
            assert!(closed, "test setup should create exactly one tcp socket");
        }

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

    #[tokio::test]
    async fn reactor_stops_when_net_is_dropped_before_start() {
        let (net, fut) = Net::new2(PendingDevice { caps: ip_caps() }, test_config());
        drop(net);

        let result = tokio::time::timeout(StdDuration::from_millis(50), fut).await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "stop request was lost: {result:?}"
        );
    }

    #[cfg(unix)]
    fn local_net_pair() -> (Net, Net) {
        use crate::device::AsyncCapture;
        use std::os::unix::net::UnixDatagram;

        let (left, right) = UnixDatagram::pair().unwrap();
        let make_net = |socket: UnixDatagram, address| {
            socket.set_nonblocking(true).unwrap();
            let capture = AsyncCapture::new(
                socket,
                |socket| {
                    let mut packet = vec![0; 1500];
                    let len = socket.recv(&mut packet)?;
                    packet.truncate(len);
                    Ok(packet)
                },
                |socket, packet| socket.send(packet).map(|_| ()),
                ip_caps(),
            )
            .unwrap();
            let mut config = test_config();
            config.ip_addr = IpCidr::new(address, 24);
            Net::new(capture, config)
        };
        (
            make_net(left, IpAddress::v4(10, 0, 0, 1)),
            make_net(right, IpAddress::v4(10, 0, 0, 2)),
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tcp_flush_returns_error_after_connection_aborts() {
        use tokio::io::AsyncWriteExt;

        tokio::time::timeout(StdDuration::from_secs(2), async {
            let (client_net, server_net) = local_net_pair();
            let address = "10.0.0.2:12345".parse().unwrap();
            let mut listener = server_net.tcp_bind(address).await.unwrap();
            let (client, server) = tokio::join!(client_net.tcp_connect(address), listener.accept());
            let mut client = client.unwrap();
            let (_server, _) = server.unwrap();

            client.write_all(b"unacknowledged").await.unwrap();
            let mut flush = Box::pin(client.flush());
            assert!(matches!(futures::poll!(&mut flush), Poll::Pending));
            {
                let mut sockets = client_net.reactor.socket_allocator().sockets().lock();
                for (_, socket) in sockets.iter_mut() {
                    if let Socket::Tcp(tcp) = socket {
                        assert!(tcp.send_queue() > 0);
                        tcp.abort();
                    }
                }
            }
            let error = flush.await.expect_err("aborted writes must fail");
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        })
        .await
        .expect("flush must not hang after an abort");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_tcp_udp_smoke() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        tokio::time::timeout(StdDuration::from_secs(10), async {
            let (client_net, server_net) = local_net_pair();
            let address = "10.0.0.2:12345".parse().unwrap();
            let mut listener = server_net.tcp_bind(address).await.unwrap();
            let options = TcpConnectOptions::default().with_source_port(23456);
            let (client, server) = tokio::join!(
                client_net.tcp_connect_with_options(address, options),
                listener.accept(),
            );
            let mut client = client.unwrap();
            let (mut server, peer) = server.unwrap();
            assert_eq!(client.local_addr().unwrap().port(), 23456);
            assert_eq!(peer.port(), 23456);

            client.write_all(b"small write").await.unwrap();
            let mut received = [0; 11];
            server.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"small write");
            client.flush().await.unwrap();

            let (mut app, mut relay_io) = tokio::io::duplex(8192);
            let relay = tokio::spawn(async move {
                tokio::io::copy_bidirectional(&mut server, &mut relay_io).await
            });
            let payload: Vec<u8> = (0..65536).map(|i| i as u8).collect();
            let mut received = vec![0; payload.len()];
            let (write, read) =
                tokio::join!(client.write_all(&payload), app.read_exact(&mut received));
            write.unwrap();
            read.unwrap();
            assert_eq!(received, payload);
            client.flush().await.unwrap();
            let (write, read) =
                tokio::join!(app.write_all(&payload), client.read_exact(&mut received));
            write.unwrap();
            read.unwrap();
            assert_eq!(received, payload);
            relay.abort();
            assert!(relay.await.unwrap_err().is_cancelled());

            let client = client_net
                .udp_bind("10.0.0.1:23457".parse().unwrap())
                .await
                .unwrap();
            let server = server_net.udp_bind(address).await.unwrap();
            let mut received = [0; 32];
            for sequence in 0u32..100 {
                let payload = sequence.to_be_bytes();
                client.send_to(&payload, address).await.unwrap();
                let (len, peer) = server.recv_from(&mut received).await.unwrap();
                assert_eq!(&received[..len], &payload);
                server.send_to(&received[..len], peer).await.unwrap();
                let (len, peer) = client.recv_from(&mut received).await.unwrap();
                assert_eq!(&received[..len], &payload);
                assert_eq!(peer, address);
            }
        })
        .await
        .expect("local TCP/UDP smoke timed out");
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
