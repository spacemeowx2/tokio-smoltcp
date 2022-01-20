use futures::{ready, Sink, Stream};
use pin_project_lite::pin_project;
use smoltcp::phy::DeviceCapabilities;
use std::{
    io,
    os::unix::io::{AsRawFd, RawFd},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{unix::AsyncFd, Interest};

use crate::device::AsyncDevice;

pin_project! {
    /// A device that uses a Unix raw socket to send and receive packets.
    /// The socket is created with the `O_NONBLOCK` flag set.
    pub struct AsyncCapture<T, R, S> {
        obj: T,
        recv: R,
        send: S,
        async_fd: AsyncFd<RawFd>,
        temp: Option<Vec<u8>>,
        poll_write: bool,
        caps: DeviceCapabilities,
    }
}

impl<T, R, S> AsyncCapture<T, R, S>
where
    T: AsRawFd,
    R: Fn(&mut T) -> io::Result<Vec<u8>>,
    S: Fn(&mut T, &[u8]) -> io::Result<()>,
{
    /// Make a new `AsyncCapture` with the given `obj` and `recv` and `send`
    /// functions.
    ///
    ///
    /// The `obj` is used to get the raw file descriptor.
    ///
    ///
    /// The `recv` and `send` functions are used to read and write packets. They should
    /// return Err(io::ErrorKind::WouldBlock) if the operation would block.
    ///
    ///
    /// The `caps` is used to determine the device capabilities. `DeviceCapabilities::max_transmission_unit` must be set.
    pub fn new(obj: T, recv: R, send: S, caps: DeviceCapabilities) -> io::Result<Self> {
        let async_fd = AsyncFd::with_interest(obj.as_raw_fd(), Interest::READABLE)?;
        Ok(AsyncCapture {
            obj,
            recv,
            send,
            async_fd,
            temp: None,
            poll_write: false,
            caps,
        })
    }
}

impl<T, R, S> Stream for AsyncCapture<T, R, S>
where
    T: AsRawFd + Send,
    R: Fn(&mut T) -> io::Result<Vec<u8>> + Send,
    S: Fn(&mut T, &[u8]) -> io::Result<()> + Send,
{
    type Item = io::Result<Vec<u8>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        let obj = &mut this.obj;
        let recv = this.recv;

        loop {
            match recv(obj) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(this.async_fd.poll_read_ready(cx))?.clear_ready()
                }
                r => return Poll::Ready(Some(r)),
            };
        }
    }
}

impl<T, R, S> Sink<Vec<u8>> for AsyncCapture<T, R, S>
where
    T: AsRawFd + Send,
    R: Fn(&mut T) -> io::Result<Vec<u8>> + Send,
    S: Fn(&mut T, &[u8]) -> io::Result<()> + Send,
{
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        assert!(self.temp.is_none());

        if self.poll_write {
            let this = self.project();

            ready!(this.async_fd.poll_write_ready(cx))?.clear_ready();
        }

        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        let mut this = self.project();
        let obj = &mut this.obj;
        let send = this.send;

        match send(obj, &item) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                *this.temp = Some(item);
                Ok(())
            }
            r => r,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if !self.poll_write {
            // drop packet
            return Poll::Ready(Ok(()));
        }
        let mut this = self.project();
        if let Some(p) = &this.temp {
            let obj = &mut this.obj;
            let send = this.send;

            loop {
                let mut guard = ready!(this.async_fd.poll_write_ready(cx))?;
                match guard.try_io(|_| send(obj, &p)) {
                    Ok(result) => {
                        this.temp.take();
                        return Poll::Ready(result);
                    }
                    Err(_) => continue,
                }
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl<T, R, S> AsyncDevice for AsyncCapture<T, R, S>
where
    T: AsRawFd + Send,
    R: Fn(&mut T) -> io::Result<Vec<u8>> + Send,
    S: Fn(&mut T, &[u8]) -> io::Result<()> + Send,
{
    fn capabilities(&self) -> &DeviceCapabilities {
        &self.caps
    }
}
