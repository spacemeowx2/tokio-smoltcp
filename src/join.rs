use crate::{device::Packet, FutureDevice};
use futures::{
    task::{Context, Poll},
    Sink, SinkExt, Stream, StreamExt,
};
use std::{net::SocketAddr, pin::Pin};
use tokio::net::UdpSocket;
use tokio_util::{codec::BytesCodec, udp::UdpFramed};

pub struct JoinInterface<T> {
    sink: Box<dyn Sink<T, Error = std::io::Error> + Send + Unpin>,
    stream: Box<dyn Stream<Item = T> + Send + Unpin>,
}

impl<T> JoinInterface<T> {
    pub fn new(
        sink: Box<dyn Sink<T, Error = std::io::Error> + Send + Unpin>,
        stream: Box<dyn Stream<Item = T> + Send + Unpin>,
    ) -> Self {
        JoinInterface {
            sink: sink,
            stream: stream,
        }
    }
}

impl<T> Stream for JoinInterface<T> {
    type Item = T;

    #[inline(always)]
    fn poll_next(
        mut self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
    ) -> Poll<Option<<Self as futures::Stream>::Item>> {
        Pin::new(&mut self.stream).poll_next(ctx)
    }
}

impl<T> Sink<T> for JoinInterface<T> {
    type Error = std::io::Error;

    #[inline(always)]
    fn poll_ready(
        mut self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
    ) -> Poll<Result<(), <Self as futures::Sink<T>>::Error>> {
        Pin::new(&mut self.sink).poll_ready(ctx)
    }

    #[inline(always)]
    fn start_send(
        mut self: Pin<&mut Self>,
        ctx: T,
    ) -> Result<(), <Self as futures::Sink<T>>::Error> {
        Pin::new(&mut self.sink).start_send(ctx)
    }

    #[inline(always)]
    fn poll_flush(
        mut self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
    ) -> Poll<Result<(), <Self as futures::Sink<T>>::Error>> {
        Pin::new(&mut self.sink).poll_flush(ctx)
    }

    #[inline(always)]
    fn poll_close(
        mut self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
    ) -> Poll<Result<(), <Self as futures::Sink<T>>::Error>> {
        Pin::new(&mut self.sink).poll_close(ctx)
    }
}

pub async fn udp_device(
    local: SocketAddr,
    remote: SocketAddr,
) -> Result<FutureDevice<JoinInterface<Packet>>, std::io::Error> {
    let socket = UdpSocket::bind(local).await?;
    let (sink, stream) = UdpFramed::new(socket, BytesCodec::new()).split();

    Ok(FutureDevice::new(
        JoinInterface::new(
            Box::new(sink.with(move |x: Packet| futures::future::ready(Ok((x.into(), remote))))),
            Box::new(stream.map(|v| v.unwrap().0.to_vec())),
        ),
        1500,
    ))
}
