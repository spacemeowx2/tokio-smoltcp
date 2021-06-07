use futures::{ready, Sink, Stream};
use pin_project_lite::pin_project;
use std::{
    future::Future,
    io,
    mem::replace,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::task::{spawn_blocking, JoinHandle};

enum Runner<F, R> {
    Temp,
    Idle(F),
    Running(JoinHandle<(R, F)>),
}

impl<F, R> Runner<F, R>
where
    F: Send + 'static,
    R: Send + 'static,
{
    fn is_idle(&self) -> bool {
        matches!(self, Runner::Idle(_))
    }
    fn start<C>(&mut self, call: C)
    where
        C: Fn(&mut F) -> R + Send + Sync + 'static,
    {
        let mut f = replace(self, Runner::Temp).get_func();
        let h = spawn_blocking(move || (call(&mut f), f));
        *self = Runner::Running(h);
    }
    fn poll_run<C>(&mut self, cx: &mut Context<'_>, call: C) -> Poll<R>
    where
        C: Fn(&mut F) -> R + Send + Sync + 'static,
    {
        match &self {
            Runner::Idle(_) => {
                let mut f = replace(self, Runner::Temp).get_func();
                let h = spawn_blocking(move || (call(&mut f), f));
                *self = Runner::Running(h);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Runner::Running(_) => {
                let mut h = replace(self, Runner::Temp).get_running();
                let (r, f) = ready!(Pin::new(&mut h).poll(cx)).unwrap();
                *self = Runner::Idle(f);
                Poll::Ready(r)
            }
            _ => unreachable!(),
        }
    }
    fn get_func(self) -> F {
        match self {
            Runner::Idle(f) => f,
            _ => unreachable!(),
        }
    }
    fn get_running(self) -> JoinHandle<(R, F)> {
        match self {
            Runner::Running(r) => r,
            _ => unreachable!(),
        }
    }
}

pin_project! {
    pub struct BlockingCapture<R, S> {
        recv: Runner<R, io::Result<Vec<u8>>>,
        send: Runner<S, io::Result<()>>,
        temp: Option<Vec<u8>>,
    }
}

impl<R, S> BlockingCapture<R, S>
where
    R: FnMut() -> io::Result<Vec<u8>> + Send + Sync + 'static,
    S: FnMut(&[u8]) -> io::Result<()> + Send + Sync + 'static,
{
    pub fn new(recv: R, send: S) -> io::Result<Self> {
        Ok(BlockingCapture {
            recv: Runner::Idle(recv),
            send: Runner::Idle(send),
            temp: None,
        })
    }
}

impl<R, S> Stream for BlockingCapture<R, S>
where
    R: FnMut() -> io::Result<Vec<u8>> + Send + Sync + 'static,
    S: FnMut(&[u8]) -> io::Result<()> + Send + Sync + 'static,
{
    type Item = io::Result<Vec<u8>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        let r = ready!(this.recv.poll_run(cx, |f: &mut R| f()));
        Poll::Ready(Some(r))
    }
}

impl<R, S> Sink<Vec<u8>> for BlockingCapture<R, S>
where
    R: FnMut() -> io::Result<Vec<u8>> + Send + Sync + 'static,
    S: FnMut(&[u8]) -> io::Result<()> + Send + Sync + 'static,
{
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        assert!(self.send.is_idle());

        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        let this = self.project();
        this.send.start(move |f| f(&item));
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        this.send.poll_run(cx, |f| f(&[]))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}
