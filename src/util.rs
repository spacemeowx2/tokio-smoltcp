#[cfg(feature = "tokio")]
pub use self::tokio::*;

#[cfg(feature = "tokio")]
mod tokio {
    use futures::{ready, Sink, Stream};
    use pin_project_lite::pin_project;
    use std::{
        io,
        os::unix::io::{AsRawFd, RawFd},
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio_crate::io::{unix::AsyncFd, Interest};

    pin_project! {
        pub struct AsyncCapture<T, R, S> {
            obj: T,
            recv: R,
            send: S,
            async_fd: AsyncFd<RawFd>,
            temp: Option<Vec<u8>>,
        }
    }

    impl<T, R, S> AsyncCapture<T, R, S>
    where
        T: AsRawFd,
        R: Fn(&mut T) -> io::Result<Vec<u8>>,
        S: Fn(&mut T, &[u8]) -> io::Result<()>,
    {
        pub fn new(obj: T, recv: R, send: S) -> io::Result<Self> {
            let async_fd = AsyncFd::with_interest(obj.as_raw_fd(), Interest::READABLE)?;
            Ok(AsyncCapture {
                obj,
                recv,
                send,
                async_fd,
                temp: None,
            })
        }
    }

    impl<T, R, S> Stream for AsyncCapture<T, R, S>
    where
        T: AsRawFd,
        R: Fn(&mut T) -> io::Result<Vec<u8>>,
        S: Fn(&mut T, &[u8]) -> io::Result<()>,
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
        T: AsRawFd,
        R: Fn(&mut T) -> io::Result<Vec<u8>>,
        S: Fn(&mut T, &[u8]) -> io::Result<()>,
    {
        type Error = io::Error;

        fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            assert!(self.temp.is_none());
            let this = self.project();

            ready!(this.async_fd.poll_write_ready(cx))?.clear_ready();

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
            let mut this = self.project();
            if let Some(p) = &this.temp {
                let obj = &mut this.obj;
                let send = this.send;

                loop {
                    let mut guard = ready!(this.async_fd.poll_read_ready(cx))?;
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

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }
}
