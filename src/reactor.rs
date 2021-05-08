use crate::device::{self, FutureDevice};
use crate::socketset::SocketSet;
use futures::{
    channel::mpsc,
    future::{self, select},
    pin_mut, FutureExt, SinkExt, StreamExt,
};
use smoltcp::{
    iface::{Interface, InterfaceBuilder},
    socket::{SocketHandle, TcpState},
    time::{Duration, Instant},
};
use std::{
    collections::HashMap,
    future::Future,
    io,
    sync::{Arc, Mutex, MutexGuard},
    task::{Poll, Waker},
};

pub struct Reactor {
    socketset: Arc<Mutex<SocketSet>>,
    sources: Arc<Mutex<HashMap<SocketHandle, Arc<Source>>>>,
    notify: mpsc::UnboundedSender<()>,
}

pub struct Wakers {
    readers: Vec<Waker>,
    writers: Vec<Waker>,
}

pub(crate) struct Source {
    wakers: Mutex<Wakers>,
}

impl Source {
    pub async fn readable(&self) -> io::Result<()> {
        let mut polled = false;

        future::poll_fn(|cx| {
            if polled {
                Poll::Ready(Ok(()))
            } else {
                let mut wakers = self.wakers.lock().unwrap();

                if wakers.readers.iter().all(|w| !w.will_wake(cx.waker())) {
                    wakers.readers.push(cx.waker().clone());
                }

                polled = true;
                Poll::Pending
            }
        })
        .await
    }
    pub async fn writable(&self) -> io::Result<()> {
        let mut polled = false;

        future::poll_fn(|cx| {
            if polled {
                Poll::Ready(Ok(()))
            } else {
                let mut wakers = self.wakers.lock().unwrap();

                if wakers.writers.iter().all(|w| !w.will_wake(cx.waker())) {
                    wakers.writers.push(cx.waker().clone());
                }

                polled = true;
                Poll::Pending
            }
        })
        .await
    }
}

async fn run<S: device::Interface + 'static>(
    mut interf: Interface<'static, FutureDevice<S>>,
    sockets: Arc<Mutex<SocketSet>>,
    sources: Arc<Mutex<HashMap<SocketHandle, Arc<Source>>>>,
    mut notify: mpsc::UnboundedReceiver<()>,
) {
    let default_timeout = Duration::from_secs(60);
    let mut ready = Vec::new();

    loop {
        let start = Instant::now();
        let deadline = {
            interf
                .poll_delay(sockets.lock().unwrap().as_set_mut(), start)
                .unwrap_or(default_timeout)
        };
        let device = interf.device_mut();

        if device.need_wait() {
            let wait = device.wait(deadline.into());
            pin_mut!(wait);
            select(wait, notify.next()).await;
        }
        let mut set = sockets.lock().unwrap();
        let end = Instant::now();
        match interf.poll(set.as_set_mut(), end) {
            Ok(true) => (),
            // readiness not changed
            Ok(false) | Err(smoltcp::Error::Dropped) => continue,
            Err(e) => {
                continue;
            }
        };

        let sources = sources.lock().unwrap();
        for socket in set.as_set_mut().iter() {
            let (readable, writable) = match socket {
                smoltcp::socket::Socket::Tcp(tcp) => (
                    tcp.can_recv() || is_going_to_close(tcp.state()),
                    tcp.can_send() || is_going_to_close(tcp.state()),
                ),
                smoltcp::socket::Socket::Raw(raw) => (raw.can_recv(), raw.can_send()),
                _ => continue, // ignore other type
            };
            let handle = socket.handle();

            if let Some(source) = sources.get(&handle) {
                let mut wakers = source.wakers.lock().unwrap();

                if readable {
                    ready.append(&mut wakers.readers);
                }

                if writable {
                    ready.append(&mut wakers.writers);
                }
            }
        }
        drop(sources);
        for waker in ready.drain(..) {
            waker.wake();
        }
    }
}

impl Reactor {
    pub fn new<S: device::Interface + 'static>(
        interf: Interface<'static, FutureDevice<S>>,
    ) -> (Self, impl Future<Output = ()> + Send) {
        let socketset = Arc::new(Mutex::new(SocketSet::new(Default::default())));
        let sources = Arc::new(Mutex::new(HashMap::new()));
        let (notify, rx) = mpsc::unbounded();
        let fut = run(interf, socketset.clone(), sources.clone(), rx);

        (
            Reactor {
                socketset,
                sources,
                notify,
            },
            fut,
        )
    }
    pub fn lock_set(&self) -> MutexGuard<'_, SocketSet> {
        self.socketset.lock().unwrap()
    }
    pub(crate) fn insert(&self, handle: SocketHandle) -> Arc<Source> {
        let mut sources = self.sources.lock().unwrap();
        let source = Arc::new(Source {
            wakers: Mutex::new(Wakers {
                readers: Vec::new(),
                writers: Vec::new(),
            }),
        });

        sources.insert(handle, source.clone());

        source
    }
    pub fn remove(&self, handle: &SocketHandle) {
        let mut sources = self.sources.lock().unwrap();

        sources.remove(handle);
    }
    pub fn notify(&self) {
        self.notify
            .clone()
            .send(())
            .now_or_never()
            .unwrap()
            .unwrap();
    }
}

fn is_going_to_close(s: TcpState) -> bool {
    match s {
        TcpState::Closed
        | TcpState::Listen
        | TcpState::SynSent
        | TcpState::SynReceived
        | TcpState::Established => false,
        _ => true,
    }
}
