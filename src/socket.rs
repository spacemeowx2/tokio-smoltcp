use super::reactor::{AcceptedTcpStream, Reactor, SocketId};
use futures::{future::{self, poll_fn}, FutureExt, Stream, ready};
#[allow(unused_imports)]
pub use smoltcp::socket::{raw, tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol, IpVersion};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::oneshot,
};

fn ep2sa(ep: &IpEndpoint) -> SocketAddr {
    match ep.addr {
        IpAddress::Ipv4(v4) => SocketAddr::new(IpAddr::V4(Ipv4Addr::from(v4)), ep.port),
        IpAddress::Ipv6(v6) => SocketAddr::new(IpAddr::V6(Ipv6Addr::from(v6)), ep.port),
        #[allow(unreachable_patterns)]
        _ => unreachable!(),
    }
}

/// A TCP socket server, listening for connections.
pub struct TcpListener {
    socket_id: SocketId,
    reactor: Arc<Reactor>,
    local_addr: SocketAddr,
    pending_accept: Option<oneshot::Receiver<io::Result<AcceptedTcpStream>>>,
}

impl TcpListener {
    pub(super) async fn new(
        reactor: Arc<Reactor>,
        local_endpoint: IpListenEndpoint,
        local_addr: SocketAddr,
    ) -> io::Result<TcpListener> {
        let created = reactor.create_tcp_listener(local_endpoint, local_addr).await?;
        Ok(TcpListener {
            socket_id: created.socket_id,
            reactor,
            local_addr,
            pending_accept: None,
        })
    }

    pub fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
        if self.pending_accept.is_none() {
            self.pending_accept = Some(self.reactor.accept(self.socket_id)?);
        }

        let mut receiver = self.pending_accept.take().expect("pending accept receiver");
        match receiver.poll_unpin(cx) {
            Poll::Pending => {
                self.pending_accept = Some(receiver);
                if let Some(error) = self.reactor.terminal_error() {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            }
            Poll::Ready(Ok(Ok(accepted))) => Poll::Ready(Ok((
                TcpStream::accepted(
                    self.reactor.clone(),
                    accepted.socket_id,
                    accepted.local_addr,
                    accepted.peer_addr,
                ),
                accepted.peer_addr,
            ))),
            Poll::Ready(Ok(Err(error))) => Poll::Ready(Err(error)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(
                self.reactor.terminal_error().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
                }),
            )),
        }
    }

    pub async fn accept(&mut self) -> io::Result<(TcpStream, SocketAddr)> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    pub fn incoming(self) -> Incoming {
        Incoming(self)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        self.reactor.drop_socket(self.socket_id);
    }
}

pub struct Incoming(TcpListener);

impl Stream for Incoming {
    type Item = io::Result<TcpStream>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let (tcp, _) = ready!(self.0.poll_accept(cx))?;
        Poll::Ready(Some(Ok(tcp)))
    }
}

/// A TCP stream between a local and a remote socket.
pub struct TcpStream {
    socket_id: SocketId,
    reactor: Arc<Reactor>,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
    connect_rx: parking_lot::Mutex<Option<oneshot::Receiver<io::Result<()>>>>,
    pending_read: Option<oneshot::Receiver<io::Result<Vec<u8>>>>,
    pending_write: Option<oneshot::Receiver<io::Result<usize>>>,
    pending_flush: Option<oneshot::Receiver<io::Result<()>>>,
    pending_shutdown: Option<oneshot::Receiver<io::Result<()>>>,
}

impl TcpStream {
    pub(super) async fn connect(
        reactor: Arc<Reactor>,
        local_endpoint: IpEndpoint,
        remote_endpoint: IpEndpoint,
    ) -> io::Result<TcpStream> {
        let local_addr = ep2sa(&local_endpoint);
        let peer_addr = ep2sa(&remote_endpoint);
        let created = reactor
            .create_tcp_stream(local_endpoint, remote_endpoint)
            .await?;

        let tcp = TcpStream {
            socket_id: created.socket_id,
            reactor,
            local_addr,
            peer_addr,
            connect_rx: parking_lot::Mutex::new(Some(created.connect)),
            pending_read: None,
            pending_write: None,
            pending_flush: None,
            pending_shutdown: None,
        };

        future::poll_fn(|cx| tcp.poll_connected(cx)).await?;
        Ok(tcp)
    }

    fn accepted(
        reactor: Arc<Reactor>,
        socket_id: SocketId,
        local_addr: SocketAddr,
        peer_addr: SocketAddr,
    ) -> TcpStream {
        TcpStream {
            socket_id,
            reactor,
            local_addr,
            peer_addr,
            connect_rx: parking_lot::Mutex::new(None),
            pending_read: None,
            pending_write: None,
            pending_flush: None,
            pending_shutdown: None,
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }

    pub fn poll_connected(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut slot = self.connect_rx.lock();
        let Some(mut receiver) = slot.take() else {
            return Poll::Ready(Ok(()));
        };

        match receiver.poll_unpin(cx) {
            Poll::Pending => {
                *slot = Some(receiver);
                if let Some(error) = self.reactor.terminal_error() {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            }
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(
                self.reactor.terminal_error().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
                }),
            )),
        }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        self.reactor.drop_socket(self.socket_id);
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        if self.pending_read.is_none() {
            self.pending_read = Some(self.reactor.tcp_read(self.socket_id, buf.remaining())?);
        }

        let mut receiver = self.pending_read.take().expect("pending read receiver");
        match receiver.poll_unpin(cx) {
            Poll::Pending => {
                self.pending_read = Some(receiver);
                if let Some(error) = self.reactor.terminal_error() {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            }
            Poll::Ready(Ok(Ok(data))) => {
                buf.put_slice(&data);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(Err(error))) => Poll::Ready(Err(error)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(
                self.reactor.terminal_error().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
                }),
            )),
        }
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if self.pending_write.is_none() {
            self.pending_write = Some(self.reactor.tcp_write(self.socket_id, buf.to_vec())?);
        }

        let mut receiver = self.pending_write.take().expect("pending write receiver");
        match receiver.poll_unpin(cx) {
            Poll::Pending => {
                self.pending_write = Some(receiver);
                if let Some(error) = self.reactor.terminal_error() {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            }
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(
                self.reactor.terminal_error().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
                }),
            )),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        if self.pending_flush.is_none() {
            self.pending_flush = Some(self.reactor.tcp_flush(self.socket_id)?);
        }

        let mut receiver = self.pending_flush.take().expect("pending flush receiver");
        match receiver.poll_unpin(cx) {
            Poll::Pending => {
                self.pending_flush = Some(receiver);
                if let Some(error) = self.reactor.terminal_error() {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            }
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(
                self.reactor.terminal_error().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
                }),
            )),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        if self.pending_shutdown.is_none() {
            self.pending_shutdown = Some(self.reactor.tcp_shutdown(self.socket_id)?);
        }

        let mut receiver = self.pending_shutdown.take().expect("pending shutdown receiver");
        match receiver.poll_unpin(cx) {
            Poll::Pending => {
                self.pending_shutdown = Some(receiver);
                if let Some(error) = self.reactor.terminal_error() {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            }
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(
                self.reactor.terminal_error().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
                }),
            )),
        }
    }
}

/// A UDP socket.
pub struct UdpSocket {
    socket_id: SocketId,
    reactor: Arc<Reactor>,
    local_addr: SocketAddr,
    pending_send: parking_lot::Mutex<Option<oneshot::Receiver<io::Result<usize>>>>,
    pending_recv: parking_lot::Mutex<Option<oneshot::Receiver<io::Result<(Vec<u8>, SocketAddr)>>>>,
}

impl UdpSocket {
    pub(super) async fn new(
        reactor: Arc<Reactor>,
        local_endpoint: IpListenEndpoint,
        local_addr: SocketAddr,
    ) -> io::Result<UdpSocket> {
        let created = reactor.create_udp_socket(local_endpoint).await?;
        Ok(UdpSocket {
            socket_id: created.socket_id,
            reactor,
            local_addr,
            pending_send: parking_lot::Mutex::new(None),
            pending_recv: parking_lot::Mutex::new(None),
        })
    }

    pub fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        let mut slot = self.pending_send.lock();
        if slot.is_none() {
            *slot = Some(self.reactor.udp_send(self.socket_id, buf.to_vec(), target)?);
        }

        let mut receiver = slot.take().expect("pending udp send receiver");
        match receiver.poll_unpin(cx) {
            Poll::Pending => {
                *slot = Some(receiver);
                if let Some(error) = self.reactor.terminal_error() {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            }
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(
                self.reactor.terminal_error().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
                }),
            )),
        }
    }

    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        poll_fn(|cx| self.poll_send_to(cx, buf, target)).await
    }

    pub fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let mut slot = self.pending_recv.lock();
        if slot.is_none() {
            *slot = Some(self.reactor.udp_recv(self.socket_id, buf.len())?);
        }

        let mut receiver = slot.take().expect("pending udp recv receiver");
        match receiver.poll_unpin(cx) {
            Poll::Pending => {
                *slot = Some(receiver);
                if let Some(error) = self.reactor.terminal_error() {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            }
            Poll::Ready(Ok(Ok((data, addr)))) => {
                let size = data.len().min(buf.len());
                buf[..size].copy_from_slice(&data[..size]);
                Poll::Ready(Ok((size, addr)))
            }
            Poll::Ready(Ok(Err(error))) => Poll::Ready(Err(error)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(
                self.reactor.terminal_error().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
                }),
            )),
        }
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| self.poll_recv_from(cx, buf)).await
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        self.reactor.drop_socket(self.socket_id);
    }
}

/// A raw socket.
pub struct RawSocket {
    socket_id: SocketId,
    reactor: Arc<Reactor>,
    pending_send: parking_lot::Mutex<Option<oneshot::Receiver<io::Result<usize>>>>,
    pending_recv: parking_lot::Mutex<Option<oneshot::Receiver<io::Result<Vec<u8>>>>>,
}

impl RawSocket {
    pub(super) async fn new(
        reactor: Arc<Reactor>,
        ip_version: IpVersion,
        ip_protocol: IpProtocol,
    ) -> io::Result<RawSocket> {
        let created = reactor.create_raw_socket(ip_version, ip_protocol).await?;
        Ok(RawSocket {
            socket_id: created.socket_id,
            reactor,
            pending_send: parking_lot::Mutex::new(None),
            pending_recv: parking_lot::Mutex::new(None),
        })
    }

    pub fn poll_send(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let mut slot = self.pending_send.lock();
        if slot.is_none() {
            *slot = Some(self.reactor.raw_send(self.socket_id, buf.to_vec())?);
        }

        let mut receiver = slot.take().expect("pending raw send receiver");
        match receiver.poll_unpin(cx) {
            Poll::Pending => {
                *slot = Some(receiver);
                if let Some(error) = self.reactor.terminal_error() {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            }
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(
                self.reactor.terminal_error().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
                }),
            )),
        }
    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        poll_fn(|cx| self.poll_send(cx, buf)).await
    }

    pub fn poll_recv(&self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let mut slot = self.pending_recv.lock();
        if slot.is_none() {
            *slot = Some(self.reactor.raw_recv(self.socket_id, buf.len())?);
        }

        let mut receiver = slot.take().expect("pending raw recv receiver");
        match receiver.poll_unpin(cx) {
            Poll::Pending => {
                *slot = Some(receiver);
                if let Some(error) = self.reactor.terminal_error() {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            }
            Poll::Ready(Ok(Ok(data))) => {
                let size = data.len().min(buf.len());
                buf[..size].copy_from_slice(&data[..size]);
                Poll::Ready(Ok(size))
            }
            Poll::Ready(Ok(Err(error))) => Poll::Ready(Err(error)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(
                self.reactor.terminal_error().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
                }),
            )),
        }
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| self.poll_recv(cx, buf)).await
    }
}

impl Drop for RawSocket {
    fn drop(&mut self) {
        self.reactor.drop_socket(self.socket_id);
    }
}
