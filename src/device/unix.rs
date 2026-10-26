use futures::{Sink, Stream, ready};
use pin_project_lite::pin_project;
use smoltcp::phy::DeviceCapabilities;
use std::{
    io,
    os::fd::{AsFd, OwnedFd},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{Interest, unix::AsyncFd};

use crate::device::AsyncDevice;

pin_project! {
    /// A device that uses a Unix raw socket to send and receive packets.
    /// The socket is created with the `O_NONBLOCK` flag set.
    pub struct AsyncCapture<T, R, S> {
        obj: T,
        recv: R,
        send: S,
        async_fd: AsyncFd<OwnedFd>,
        temp: Option<Vec<u8>>,
        poll_write: bool,
        caps: DeviceCapabilities,
    }
}

impl<T, R, S> AsyncCapture<T, R, S>
where
    T: AsFd,
    R: Fn(&mut T) -> io::Result<Vec<u8>>,
    S: Fn(&mut T, &[u8]) -> io::Result<()>,
{
    /// Make a new `AsyncCapture` with the given `obj` and `recv` and `send`
    /// functions.
    ///
    ///
    /// The object must implement `AsFd`; a duplicate descriptor is owned until deregistration.
    ///
    ///
    /// The `recv` and `send` functions are used to read and write packets. They should
    /// return Err(io::ErrorKind::WouldBlock) if the operation would block.
    ///
    ///
    /// The `caps` is used to determine the device capabilities. `DeviceCapabilities::max_transmission_unit` must be set.
    pub fn new(obj: T, recv: R, send: S, caps: DeviceCapabilities) -> io::Result<Self> {
        Self::new_with_interest(
            obj,
            recv,
            send,
            caps,
            Interest::READABLE | Interest::WRITABLE,
        )
    }

    /// Make a new `AsyncCapture` with an explicit readiness interest.
    pub fn new_with_interest(
        obj: T,
        recv: R,
        send: S,
        caps: DeviceCapabilities,
        interest: Interest,
    ) -> io::Result<Self> {
        let fd = obj.as_fd().try_clone_to_owned()?;
        // SAFETY: AsyncFd owns this duplicate, which callbacks cannot close or replace.
        let async_fd = unsafe { AsyncFd::register_with_interest(fd, interest) }?;
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
    T: AsFd + Send,
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
    T: AsFd + Send,
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
                *this.poll_write = true;
                *this.temp = Some(item);
                Ok(())
            }
            r => {
                *this.poll_write = false;
                r
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut this = self.project();

        if !*this.poll_write {
            return Poll::Ready(Ok(()));
        }

        if let Some(p) = &this.temp {
            let obj = &mut this.obj;
            let send = this.send;

            loop {
                let mut guard = ready!(this.async_fd.poll_write_ready(cx))?;
                match guard.try_io(|_| send(obj, p)) {
                    Ok(result) => {
                        this.temp.take();
                        *this.poll_write = false;
                        return Poll::Ready(result);
                    }
                    Err(_) => continue,
                }
            }
        } else {
            *this.poll_write = false;
            Poll::Ready(Ok(()))
        }
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl<T, R, S> AsyncDevice for AsyncCapture<T, R, S>
where
    T: AsFd + Send,
    R: Fn(&mut T) -> io::Result<Vec<u8>> + Send,
    S: Fn(&mut T, &[u8]) -> io::Result<()> + Send,
{
    fn capabilities(&self) -> &DeviceCapabilities {
        &self.caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };

    #[tokio::test]
    async fn read_only_capture_wakes_on_incoming_data() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut capture = AsyncCapture::new_with_interest(
            stream,
            |stream| {
                let mut packet = vec![0; 16];
                let len = stream.read(&mut packet)?;
                packet.truncate(len);
                Ok(packet)
            },
            |stream, packet| stream.write_all(packet),
            DeviceCapabilities::default(),
            Interest::READABLE,
        )
        .unwrap();

        let mut receive = Box::pin(capture.next());
        assert!(matches!(futures::poll!(&mut receive), Poll::Pending));
        peer.write_all(b"ready").unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(1), receive)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(packet, b"ready");
    }

    #[tokio::test]
    async fn registration_owns_fd_until_capture_is_dropped() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut capture = AsyncCapture::new(
            stream,
            |_| Err(io::ErrorKind::WouldBlock.into()),
            |_, _| Ok(()),
            DeviceCapabilities::default(),
        )
        .unwrap();

        // Callbacks can replace the original object; registration must keep its FD alive.
        let (replacement, _peer) = UnixStream::pair().unwrap();
        capture.obj = replacement;
        assert_eq!(
            peer.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(capture);
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}
