use super::{
    reactor::Source,
    NetReactor,
    SocketSet,
};
pub use smoltcp::socket::{self, SocketHandle, SocketRef, TcpState, AnySocket};
use std::sync::Mutex;
use futures::{future::poll_fn, pin_mut};

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    mem::replace,
    future::Future,
    net::SocketAddr,
};
use tokio::io::{self, AsyncRead, AsyncWrite};
use futures::{ready, Stream};

#[derive(Clone)]
struct Base {
    handle: SocketHandle,
    reactor: Arc<NetReactor>,
    source: Arc<Source>,
}

impl Base {
    fn new<F>(reactor: Arc<NetReactor>, f: F) -> Base
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
        T: AnySocket<'static, 'static>,
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
            self.source.writable(&self.reactor).await?;
        }
    }
    async fn readable<T, F, R>(&self, mut f: F) -> io::Result<R>
    where
        T: AnySocket<'static, 'static>,
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
            self.source.readable(&self.reactor).await?;
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
    pub(super) async fn new(reactor: Arc<NetReactor>) -> TcpListener {
        TcpListener {
            base: Base::new(reactor, SocketSet::new_tcp_socket),
        }
    }
    pub async fn accept(&mut self) -> io::Result<TcpSocket> {
        self.base.writable(|socket: &mut SocketRef<socket::TcpSocket>| {
            if socket.can_send() {
                drop(socket);
                return Some(Ok(()))
            }
            None
        }).await?;
        Ok(TcpSocket::new(&mut self.base))
    }
    pub fn incoming(self) -> Incoming {
        Incoming(self)
    }
}

pub struct Incoming(TcpListener);

impl Stream for Incoming {
    type Item = TcpSocket;
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let fut = self.0.accept();
        futures::pin_mut!(fut);
        let r = ready!(fut.poll(cx));
        Poll::Ready(r.ok())
    }
}

pub struct SendHalf {
    inner: Arc<Mutex<UdpSocket>>,
}
pub struct RecvHalf {
    inner: Arc<Mutex<UdpSocket>>,
}

impl SendHalf {
    pub async fn send(&mut self, data: &OwnedUdp) -> io::Result<()> {
        poll_fn(move |cx| {
            let mut inner = self.inner.lock().unwrap();
            let fut = inner.send(data);
            pin_mut!(fut);
            fut.poll(cx)
        }).await
    }
}

impl RecvHalf {
    pub async fn recv(&mut self) -> io::Result<OwnedUdp> {
        poll_fn(|cx| {
            let mut inner = self.inner.lock().unwrap();
            let fut = inner.recv();
            pin_mut!(fut);
            fut.poll(cx)
        }).await
    }
}

impl UdpSocket {
    pub(super) async fn new(reactor: Arc<NetReactor>) -> UdpSocket {
        UdpSocket {
            base: Base::new(reactor, SocketSet::new_raw_socket)
        }
    }
    pub async fn recv(&mut self) -> io::Result<OwnedUdp> {
        self.base.readable(|socket: &mut SocketRef<socket::RawSocket>| {
            if socket.can_recv() {
                return Some(socket
                    .recv()
                    .map(|p| parse_udp_owned(p, &ChecksumCapabilities::default()))
                    .and_then(|x| x)
                    .map_err(map_err));
            }
            None
        }).await
    }
    pub async fn send(&mut self, data: &OwnedUdp) -> io::Result<()> {
        self.base.writable(|socket: &mut SocketRef<socket::RawSocket>| {
            if socket.can_send() {
                let r = socket.send_slice(&data.to_raw())
                    .map_err(map_err);
                self.base.reactor.notify();
                return Some(r);
            }
            None
        }).await
    }
    pub fn split(self) -> (SendHalf, RecvHalf) {
        let inner = Arc::new(Mutex::new(self));
        (SendHalf {
            inner: inner.clone(),
        }, RecvHalf {
            inner,
        })
    }
}

impl TcpSocket {
    fn new(listener_base: &mut Base) -> TcpSocket {
        let base = listener_base.accept(SocketSet::new_tcp_socket);

        let mut set = base.lock_set();
        let socket = set.get::<socket::TcpSocket>(base.handle);

        let local_addr = endpoint2socketaddr(&socket.local_endpoint());
        let peer_addr = endpoint2socketaddr(&socket.remote_endpoint());
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
        self.base.readable(|socket: &mut SocketRef<socket::TcpSocket>| {
            if socket.state() != TcpState::Established {
                socket.close();
                return Some(Ok(0))
            }
            if socket.can_recv() {
                return Some(socket
                    .recv_slice(buf)
                    .map_err(map_err));
            }
            None
        }).await
    }
    pub async fn send(&mut self, data: &[u8]) -> io::Result<usize> {
        self.base.writable(|socket: &mut SocketRef<socket::TcpSocket>| {
            if socket.state() != TcpState::Established {
                socket.close();
                return Some(Ok(0))
            }
            if socket.can_send() {
                let r = socket.send_slice(data)
                    .map_err(map_err);
                self.base.reactor.notify();
                return Some(r);
            }
            None
        }).await
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
        buf.set_filled(set);
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
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let fut = self.shutdown();
        futures::pin_mut!(fut);
        fut.poll(cx)
    }
}
