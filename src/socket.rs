use super::{
    reactor::{Reactor, Source},
    SocketSet,
};
pub use smoltcp::socket::{self, AnySocket, SocketHandle, SocketRef, TcpState};
use smoltcp::wire::{IpAddress, IpEndpoint};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use futures::{ready, Stream};
use std::{
    future::Future,
    mem::replace,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{self, AsyncRead, AsyncWrite};

#[derive(Clone)]
struct Base {
    handle: SocketHandle,
    reactor: Arc<Reactor>,
    source: Arc<Source>,
}

impl Base {
    fn new<F>(reactor: Arc<Reactor>, f: F) -> Base
    where
        F: FnOnce(&mut SocketSet) -> SocketHandle,
    {
        let mut set = reactor.lock_set();
        let handle = f(&mut set);
        drop(set);
        let source = reactor.insert(handle);

        Base {
            handle,
            reactor,
            source,
        }
    }
    fn lock_set(&self) -> std::sync::MutexGuard<'_, SocketSet> {
        self.reactor.lock_set()
    }
    async fn connect(
        &mut self,
        local_endpoint: IpEndpoint,
        remote_endpoint: IpEndpoint,
    ) -> io::Result<()> {
        {
            let mut set = self.lock_set();
            let mut socket = set.get::<socket::TcpSocket>(self.handle);
            socket
                .connect(remote_endpoint, local_endpoint)
                .map_err(map_err)?;
        }
        self.writable(|socket: &mut SocketRef<socket::TcpSocket>| {
            if socket.state() == TcpState::Established {
                Some(Ok(()))
            } else {
                None
            }
        })
        .await
    }
    fn accept<F>(&mut self, f: F) -> Base
    where
        F: FnOnce(&mut SocketSet) -> SocketHandle,
    {
        let reactor = self.reactor.clone();
        let mut set = reactor.lock_set();
        let handle = f(&mut set);
        drop(set);
        let source = reactor.insert(handle);

        Base {
            reactor,
            handle: replace(&mut self.handle, handle),
            source: replace(&mut self.source, source),
        }
    }
    async fn writable<T, F, R>(&self, mut f: F) -> io::Result<R>
    where
        T: AnySocket<'static>,
        F: FnMut(&mut SocketRef<T>) -> Option<io::Result<R>>,
    {
        loop {
            {
                let mut set = self.lock_set();
                let mut socket = set.get::<T>(self.handle);
                match f(&mut socket) {
                    Some(r) => return r,
                    None => (),
                };
            }
            self.source.writable().await?;
        }
    }
    async fn readable<T, F, R>(&self, mut f: F) -> io::Result<R>
    where
        T: AnySocket<'static>,
        F: FnMut(&mut SocketRef<T>) -> Option<io::Result<R>>,
    {
        loop {
            {
                let mut set = self.lock_set();
                let mut socket = set.get::<T>(self.handle);
                match f(&mut socket) {
                    Some(r) => return r,
                    None => (),
                };
            }
            self.source.readable().await?;
        }
    }
}

impl Drop for Base {
    fn drop(&mut self) {
        self.reactor.remove(&self.handle);
        let mut set = self.reactor.lock_set();
        set.remove(self.handle);
    }
}

pub struct TcpListener {
    base: Base,
}

pub struct TcpSocket {
    base: Base,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}

pub struct UdpSocket {
    base: Base,
}

fn map_err(e: smoltcp::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

impl TcpListener {
    pub(super) async fn new(reactor: Arc<Reactor>) -> TcpListener {
        TcpListener {
            base: Base::new(reactor, SocketSet::new_tcp_socket),
        }
    }
    pub async fn accept(&mut self) -> io::Result<TcpSocket> {
        self.base
            .writable(|socket: &mut SocketRef<socket::TcpSocket>| {
                if socket.can_send() {
                    drop(socket);
                    return Some(Ok(()));
                }
                None
            })
            .await?;
        Ok(TcpSocket::new(&mut self.base))
    }
    pub fn incoming(self) -> Incoming {
        Incoming(self)
    }
}

pub struct Incoming(TcpListener);

impl Stream for Incoming {
    type Item = TcpSocket;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let fut = self.0.accept();
        futures::pin_mut!(fut);
        let r = ready!(fut.poll(cx));
        Poll::Ready(r.ok())
    }
}

impl UdpSocket {
    pub(super) async fn new(reactor: Arc<Reactor>) -> UdpSocket {
        UdpSocket {
            base: Base::new(reactor, SocketSet::new_raw_socket),
        }
    }
    // pub async fn recv(&mut self) -> io::Result<OwnedUdp> {
    //     self.base
    //         .readable(|socket: &mut SocketRef<socket::RawSocket>| {
    //             if socket.can_recv() {
    //                 return Some(
    //                     socket
    //                         .recv()
    //                         .map(|p| parse_udp_owned(p, &ChecksumCapabilities::default()))
    //                         .and_then(|x| x)
    //                         .map_err(map_err),
    //                 );
    //             }
    //             None
    //         })
    //         .await
    // }
    // pub async fn send(&mut self, data: &OwnedUdp) -> io::Result<()> {
    //     self.base
    //         .writable(|socket: &mut SocketRef<socket::RawSocket>| {
    //             if socket.can_send() {
    //                 let r = socket.send_slice(&data.to_raw()).map_err(map_err);
    //                 self.base.reactor.notify();
    //                 return Some(r);
    //             }
    //             None
    //         })
    //         .await
    // }
    // pub fn split(self) -> (SendHalf, RecvHalf) {
    //     let inner = Arc::new(Mutex::new(self));
    //     (
    //         SendHalf {
    //             inner: inner.clone(),
    //         },
    //         RecvHalf { inner },
    //     )
    // }
}

fn ep2sa(ep: &IpEndpoint) -> SocketAddr {
    match ep.addr {
        IpAddress::Ipv4(v4) => SocketAddr::new(IpAddr::V4(Ipv4Addr::from(v4)), ep.port),
        IpAddress::Ipv6(v6) => SocketAddr::new(IpAddr::V6(Ipv6Addr::from(v6)), ep.port),
        _ => unreachable!(),
    }
}

impl TcpSocket {
    pub(super) async fn connect(
        reactor: Arc<Reactor>,
        local_endpoint: IpEndpoint,
        remote_endpoint: IpEndpoint,
    ) -> io::Result<TcpSocket> {
        let mut base = Base::new(reactor, SocketSet::new_tcp_socket);
        base.connect(local_endpoint, remote_endpoint).await?;
        let local_addr = ep2sa(&local_endpoint);
        let peer_addr = ep2sa(&remote_endpoint);
        Ok(TcpSocket {
            base,
            local_addr,
            peer_addr,
        })
    }

    fn new(listener_base: &mut Base) -> TcpSocket {
        let base = listener_base.accept(SocketSet::new_tcp_socket);
        // TODO: set to listen here

        let mut set = base.lock_set();
        let socket = set.get::<socket::TcpSocket>(base.handle);

        let local_addr = ep2sa(&socket.local_endpoint());
        let peer_addr = ep2sa(&socket.remote_endpoint());
        drop(socket);
        drop(set);

        TcpSocket {
            base,
            local_addr,
            peer_addr,
        }
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }
    pub async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.base
            .readable(|socket: &mut SocketRef<socket::TcpSocket>| {
                if !socket.may_recv() {
                    socket.close();
                    return Some(Ok(0));
                }
                if socket.can_recv() {
                    return Some(socket.recv_slice(buf).map_err(map_err));
                }
                None
            })
            .await
    }
    pub async fn send(&mut self, data: &[u8]) -> io::Result<usize> {
        self.base
            .writable(|socket: &mut SocketRef<socket::TcpSocket>| {
                if !socket.may_send() {
                    socket.close();
                    return Some(Ok(0));
                }
                if socket.can_send() {
                    let r = socket.send_slice(data).map_err(map_err);
                    self.base.reactor.notify();
                    return Some(r);
                }
                None
            })
            .await
    }
    pub async fn shutdown(&mut self) -> io::Result<()> {
        let mut set = self.base.lock_set();
        let mut socket = set.get::<socket::TcpSocket>(self.base.handle);
        socket.close();
        Ok(())
    }
}

impl AsyncRead for TcpSocket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let set = {
            let fut = self.recv(buf.initialize_unfilled());
            futures::pin_mut!(fut);
            futures::ready!(fut.poll(cx))?
        };
        buf.advance(set);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for TcpSocket {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let fut = self.send(buf);
        futures::pin_mut!(fut);
        fut.poll(cx)
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let fut = self.shutdown();
        futures::pin_mut!(fut);
        fut.poll(cx)
    }
}
