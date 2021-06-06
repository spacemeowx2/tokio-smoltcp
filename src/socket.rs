use super::{reactor::Reactor, socket_alloctor::SocketHandle};
use futures::{future, ready};
pub use smoltcp::socket::{self, AnySocket, SocketRef, TcpState};
use smoltcp::wire::{IpAddress, IpEndpoint};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// async fn connect(
//     &mut self,
//     local_endpoint: IpEndpoint,
//     remote_endpoint: IpEndpoint,
// ) -> io::Result<()> {
//     {
//         let mut set = self.lock_set();
//         let mut socket = set.get::<socket::TcpSocket>(self.handle);
//         socket
//             .connect(remote_endpoint, local_endpoint)
//             .map_err(map_err)?;
//     }
//     self.writable(|socket: &mut SocketRef<socket::TcpSocket>| {
//         if socket.state() == TcpState::Established {
//             Some(Ok(()))
//         } else {
//             None
//         }
//     })
//     .await
// }
// fn accept<F>(&mut self, f: F) -> Base
// where
//     F: FnOnce(&mut SocketSet) -> SocketHandle,
// {
//     let reactor = self.reactor.clone();
//     let mut set = reactor.lock_set();
//     let handle = f(&mut set);
//     drop(set);

//     Base {
//         reactor,
//         handle: replace(&mut self.handle, handle),
//     }
// }
// fn poll_write<T, F, R>(&self, mut f: F) -> Poll<io::Result<R>>
// where
//     T: AnySocket<'static>,
//     F: FnMut(&mut SocketRef<T>) -> Option<io::Result<R>>,
// {
//     let mut set = self.lock_set();
//     let mut socket = set.get::<T>(self.handle);
//     match f(&mut socket) {
//         Some(r) => Poll::Ready(r),
//         None => Poll::Pending,
//     }
// }
// }

// pub struct TcpListener {
//     base: Base,
// }

fn map_err(e: smoltcp::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

// impl TcpListener {
//     pub(super) async fn new(reactor: Arc<Reactor>) -> TcpListener {
//         TcpListener {
//             base: Base::new(reactor, SocketAlloctor::new_tcp_socket),
//         }
//     }
//     // pub async fn accept(&mut self) -> io::Result<TcpSocket> {
//     //     self.base
//     //         .writable(|socket: &mut SocketRef<socket::TcpSocket>| {
//     //             if socket.can_send() {
//     //                 drop(socket);
//     //                 return Some(Ok(()));
//     //             }
//     //             None
//     //         })
//     //         .await?;
//     //     Ok(TcpSocket::new(&mut self.base))
//     // }
//     pub fn incoming(self) -> Incoming {
//         Incoming(self)
//     }
// }

// pub struct Incoming(TcpListener);

// impl Stream for Incoming {
//     type Item = TcpSocket;
//     fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
//         let fut = self.0.accept();
//         futures::pin_mut!(fut);
//         let r = ready!(fut.poll(cx));
//         Poll::Ready(r.ok())
//     }
// }

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
            let mut set = reactor.socket_alloctor().as_mut();
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

    // fn new(listener_base: &mut Base) -> TcpSocket {
    //     let base = listener_base.accept(SocketSet::new_tcp_socket);
    //     // TODO: set to listen here

    //     let mut set = base.lock_set();
    //     let socket = set.get::<socket::TcpSocket>(base.handle);

    //     let local_addr = ep2sa(&socket.local_endpoint());
    //     let peer_addr = ep2sa(&socket.remote_endpoint());
    //     drop(socket);
    //     drop(set);

    //     TcpSocket {
    //         base,
    //         local_addr,
    //         peer_addr,
    //     }
    // }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut set = self.reactor.socket_alloctor().as_mut();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);
        if socket.can_recv() {
            return Poll::Ready(Ok(()));
        }
        socket.register_recv_waker(cx.waker());
        Poll::Pending
    }
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut set = self.reactor.socket_alloctor().as_mut();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);
        if socket.can_send() {
            return Poll::Ready(Ok(()));
        }
        socket.register_send_waker(cx.waker());
        Poll::Pending
    }
    pub fn poll_connected(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut set = self.reactor.socket_alloctor().as_mut();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);

        loop {
            ready!(self.poll_read_ready(cx))?;
            if socket.state() == TcpState::Established {
                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl AsyncRead for TcpSocket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut set = self.reactor.socket_alloctor().as_mut();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);
        if !socket.may_recv() {
            return Poll::Ready(Ok(()));
        }
        loop {
            ready!(self.poll_read_ready(cx))?;
            if socket.can_recv() {
                let read = socket
                    .recv_slice(buf.initialize_unfilled())
                    .map_err(map_err)?;
                buf.advance(read);
                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl AsyncWrite for TcpSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let mut set = self.reactor.socket_alloctor().as_mut();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);
        if !socket.may_send() {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        loop {
            ready!(self.poll_write_ready(cx))?;
            if socket.can_send() {
                let r = socket.send_slice(buf).map_err(map_err)?;
                self.reactor.notify();
                return Poll::Ready(Ok(r));
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let mut set = self.reactor.socket_alloctor().as_mut();
        let socket = set.get::<socket::TcpSocket>(*self.handle);
        loop {
            ready!(self.poll_write_ready(cx))?;
            if socket.send_queue() == 0 {
                return Poll::Ready(Ok(()));
            }
        }
    }
    // TODO finish shutdown
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let mut set = self.reactor.socket_alloctor().as_mut();
        let mut socket = set.get::<socket::TcpSocket>(*self.handle);
        socket.close();
        Poll::Ready(Ok(()))
    }
}
