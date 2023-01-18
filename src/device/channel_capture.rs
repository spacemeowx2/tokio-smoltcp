use futures::{Sink, Stream};
use smoltcp::phy::DeviceCapabilities;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_util::sync::{PollSendError, PollSender};

use crate::device::AsyncDevice;

/// A device that send and receive packets using a channel.
pub struct ChannelCapture {
    recv: Receiver<io::Result<Vec<u8>>>,
    send: PollSender<Vec<u8>>,
    caps: DeviceCapabilities,
}

impl ChannelCapture {
    /// Make a new `ChannelCapture` with the given `recv` and `send` channels.
    ///
    /// The `caps` is used to determine the device capabilities. `DeviceCapabilities::max_transmission_unit` must be set.
    pub fn new<R, S>(recv: R, send: S, caps: DeviceCapabilities) -> Self
    where
        S: FnOnce(Receiver<Vec<u8>>) + Send + 'static,
        R: FnOnce(Sender<io::Result<Vec<u8>>>) + Send + 'static,
    {
        let (tx1, rx1) = channel(1000);
        let (tx2, rx2) = channel(1000);
        std::thread::spawn(move || {
            recv(tx2);
            eprintln!("Recv thread exited")
        });
        std::thread::spawn(move || {
            send(rx1);
            eprintln!("Send thread exited")
        });
        ChannelCapture {
            send: PollSender::new(tx1),
            recv: rx2,
            caps,
        }
    }
}

impl Stream for ChannelCapture {
    type Item = io::Result<Vec<u8>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.recv.poll_recv(cx)
    }
}

fn map_err(e: PollSendError<Vec<u8>>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

impl Sink<Vec<u8>> for ChannelCapture {
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.send.poll_reserve(cx).map_err(map_err)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        self.send.send_item(item).map_err(map_err)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.send.poll_reserve(cx).map_err(map_err)
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncDevice for ChannelCapture {
    fn capabilities(&self) -> &DeviceCapabilities {
        &self.caps
    }
}
