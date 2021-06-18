use super::{reactor::Reactor, socket_alloctor::SocketHandle};
use futures::future::{self, poll_fn};
use futures::{ready, Stream};
pub use smoltcp::socket::{self, AnySocket, SocketRef, TcpState};
use smoltcp::wire::{IpAddress, IpEndpoint};
use std::mem::replace;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct TcpListener {
    handle: SocketHandle,
    reactor: Arc<Reactor>,
    local_addr: SocketAddr,
}

fn map_err(e: smoltcp::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

impl TcpListener {
    pub(super) async fn new(
        reactor: Arc<Reactor>,
        local_endpoint: IpEndpoint,
    ) -> io::Result<TcpListener> {
        let handle = reactor.socket_alloctor().new_tcp_socket();
        {
            let mut set = reactor.socket_alloctor().lock();
            let mut socket = set.get::<socket::TcpSocket>(*handle);
            socket.listen(local_endpoint).map_err(map_err)?;
        }

        let local_addr = ep2sa(&local_endpoint);
        Ok(TcpListener {
            handle,
            reactor,
            local_addr,
        })
    }
    pub fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(TcpSocket, SocketAddr)>> {
        let mut set = self.reactor.socket_alloctor().lock();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);

        if socket.state() == TcpState::Established {
            drop(socket);
            drop(set);
            eprintln!("accepted ");
            return Poll::Ready(Ok(TcpSocket::accept(self)?));
        }
        socket.register_send_waker(cx.waker());
        Poll::Pending
    }
    pub async fn accept(&mut self) -> io::Result<(TcpSocket, SocketAddr)> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }
    pub fn incoming(self) -> Incoming {
        Incoming(self)
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

pub struct Incoming(TcpListener);

impl Stream for Incoming {
    type Item = io::Result<TcpSocket>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let (tcp, _) = ready!(self.0.poll_accept(cx))?;
        Poll::Ready(Some(Ok(tcp)))
    }
}

fn ep2sa(ep: &IpEndpoint) -> SocketAddr {
    match ep.addr {
        IpAddress::Ipv4(v4) => SocketAddr::new(IpAddr::V4(Ipv4Addr::from(v4)), ep.port),
        IpAddress::Ipv6(v6) => SocketAddr::new(IpAddr::V6(Ipv6Addr::from(v6)), ep.port),
        _ => unreachable!(),
    }
}

pub struct TcpSocket {
    handle: SocketHandle,
    reactor: Arc<Reactor>,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}

impl TcpSocket {
    pub(super) async fn connect(
        reactor: Arc<Reactor>,
        local_endpoint: IpEndpoint,
        remote_endpoint: IpEndpoint,
    ) -> io::Result<TcpSocket> {
        let handle = reactor.socket_alloctor().new_tcp_socket();
        {
            let mut set = reactor.socket_alloctor().lock();
            let mut socket = set.get::<socket::TcpSocket>(*handle);
            socket
                .connect(remote_endpoint, local_endpoint)
                .map_err(map_err)?;
        }

        let local_addr = ep2sa(&local_endpoint);
        let peer_addr = ep2sa(&remote_endpoint);
        let tcp = TcpSocket {
            handle,
            reactor,
            local_addr,
            peer_addr,
        };

        future::poll_fn(|cx| tcp.poll_connected(cx)).await?;

        Ok(tcp)
    }

    fn accept(listener: &mut TcpListener) -> io::Result<(TcpSocket, SocketAddr)> {
        let reactor = listener.reactor.clone();
        let new_handle = reactor.socket_alloctor().new_tcp_socket();
        let mut set = reactor.socket_alloctor().lock();
        {
            let mut new_socket = set.get::<socket::TcpSocket>(*new_handle);
            new_socket.listen(listener.local_addr).map_err(map_err)?;
        }
        let (peer_addr, local_addr) = {
            let socket = set.get::<socket::TcpSocket>(*listener.handle);
            (
                ep2sa(&socket.remote_endpoint()),
                ep2sa(&socket.local_endpoint()),
            )
        };

        Ok((
            TcpSocket {
                handle: replace(&mut listener.handle, new_handle),
                reactor: reactor.clone(),
                local_addr,
                peer_addr,
            },
            peer_addr,
        ))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }
    pub fn poll_connected(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut set = self.reactor.socket_alloctor().lock();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);
        if socket.state() == TcpState::Established {
            return Poll::Ready(Ok(()));
        }
        socket.register_send_waker(cx.waker());
        Poll::Pending
    }
}

impl AsyncRead for TcpSocket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut set = self.reactor.socket_alloctor().lock();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);
        if !socket.may_recv() {
            return Poll::Ready(Ok(()));
        }
        if socket.can_recv() {
            let read = socket
                .recv_slice(buf.initialize_unfilled())
                .map_err(map_err)?;
            buf.advance(read);
            return Poll::Ready(Ok(()));
        }
        socket.register_recv_waker(cx.waker());
        Poll::Pending
    }
}

impl AsyncWrite for TcpSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let mut set = self.reactor.socket_alloctor().lock();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);
        if !socket.may_send() {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if socket.can_send() {
            let r = socket.send_slice(buf).map_err(map_err)?;
            self.reactor.notify();
            return Poll::Ready(Ok(r));
        }
        socket.register_send_waker(cx.waker());
        Poll::Pending
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let mut set = self.reactor.socket_alloctor().lock();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);
        if socket.send_queue() == 0 {
            return Poll::Ready(Ok(()));
        }
        socket.register_send_waker(cx.waker());
        Poll::Pending
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let mut set = self.reactor.socket_alloctor().lock();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);

        if socket.is_open() {
            socket.close();
            self.reactor.notify();
        }
        if socket.state() == TcpState::Closed {
            return Poll::Ready(Ok(()));
        }

        socket.register_send_waker(cx.waker());
        Poll::Pending
    }
}

pub struct UdpSocket {
    handle: SocketHandle,
    reactor: Arc<Reactor>,
    local_addr: SocketAddr,
}

impl UdpSocket {
    pub(super) async fn new(
        reactor: Arc<Reactor>,
        local_endpoint: IpEndpoint,
    ) -> io::Result<UdpSocket> {
        let handle = reactor.socket_alloctor().new_udp_socket();
        {
            let mut set = reactor.socket_alloctor().lock();
            let mut socket = set.get::<socket::UdpSocket>(*handle);
            socket.bind(local_endpoint).map_err(map_err)?;
        }

        let local_addr = ep2sa(&local_endpoint);

        Ok(UdpSocket {
            handle,
            reactor,
            local_addr,
        })
    }
    /// Note that on multiple calls to a poll_* method in the send direction, only the Waker from the Context passed to the most recent call will be scheduled to receive a wakeup.
    pub fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        let mut set = self.reactor.socket_alloctor().lock();
        let mut socket = set.get::<socket::UdpSocket>(*self.handle);

        match socket.send_slice(buf, target.into()) {
            // the buffer is full
            Err(smoltcp::Error::Truncated) => {}
            r => {
                r.map_err(map_err)?;
                self.reactor.notify();
                return Poll::Ready(Ok(buf.len()));
            }
        }

        socket.register_send_waker(cx.waker());
        Poll::Pending
    }
    /// See note on `poll_send_to`
    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        poll_fn(|cx| self.poll_send_to(cx, buf, target)).await
    }
    /// Note that on multiple calls to a poll_* method in the recv direction, only the Waker from the Context passed to the most recent call will be scheduled to receive a wakeup.
    pub fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let mut set = self.reactor.socket_alloctor().lock();
        let mut socket = set.get::<socket::UdpSocket>(*self.handle);

        match socket.recv_slice(buf) {
            // the buffer is empty
            Err(smoltcp::Error::Exhausted) => {}
            r => {
                let (size, endpoint) = r.map_err(map_err)?;
                return Poll::Ready(Ok((size, ep2sa(&endpoint))));
            }
        }

        socket.register_recv_waker(cx.waker());
        Poll::Pending
    }
    /// See note on `poll_recv_from`
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| self.poll_recv_from(cx, buf)).await
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

pub struct RawSocket {
    handle: SocketHandle,
    reactor: Arc<Reactor>,
    local_addr: SocketAddr,
}

impl RawSocket {
    pub(super) async fn new(
        reactor: Arc<Reactor>,
        local_endpoint: IpEndpoint,
    ) -> io::Result<RawSocket> {
        let handle = reactor.socket_alloctor().new_udp_socket();
        {
            let mut set = reactor.socket_alloctor().lock();
            let mut socket = set.get::<socket::RawSocket>(*handle);
        }

        let local_addr = ep2sa(&local_endpoint);

        Ok(RawSocket {
            handle,
            reactor,
            local_addr,
        })
    }
    /// Note that on multiple calls to a poll_* method in the send direction, only the Waker from the Context passed to the most recent call will be scheduled to receive a wakeup.
    pub fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        let mut set = self.reactor.socket_alloctor().lock();
        let mut socket = set.get::<socket::UdpSocket>(*self.handle);

        match socket.send_slice(buf, target.into()) {
            // the buffer is full
            Err(smoltcp::Error::Truncated) => {}
            r => {
                r.map_err(map_err)?;
                self.reactor.notify();
                return Poll::Ready(Ok(buf.len()));
            }
        }

        socket.register_send_waker(cx.waker());
        Poll::Pending
    }
    /// See note on `poll_send_to`
    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        poll_fn(|cx| self.poll_send_to(cx, buf, target)).await
    }
    /// Note that on multiple calls to a poll_* method in the recv direction, only the Waker from the Context passed to the most recent call will be scheduled to receive a wakeup.
    pub fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let mut set = self.reactor.socket_alloctor().lock();
        let mut socket = set.get::<socket::UdpSocket>(*self.handle);

        match socket.recv_slice(buf) {
            // the buffer is empty
            Err(smoltcp::Error::Exhausted) => {}
            r => {
                let (size, endpoint) = r.map_err(map_err)?;
                return Poll::Ready(Ok((size, ep2sa(&endpoint))));
            }
        }

        socket.register_recv_waker(cx.waker());
        Poll::Pending
    }
    /// See note on `poll_recv_from`
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| self.poll_recv_from(cx, buf)).await
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}
