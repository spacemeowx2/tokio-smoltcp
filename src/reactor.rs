use crate::device::{self, FutureDevice};
use crate::socketset::SocketSet;
use futures::{channel::mpsc, future::select, pin_mut, StreamExt};
use smoltcp::{
    iface::{Interface, InterfaceBuilder},
    socket::{SocketHandle, TcpState},
    time::{Duration, Instant},
};
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    task::Waker,
};

pub struct Net {
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

impl Net {
    pub fn new<S: device::Interface + 'static>(
        device: FutureDevice<S>,
    ) -> (Self, impl Future<Output = ()>) {
        let socketset = Arc::new(Mutex::new(SocketSet::new(Default::default())));
        let sources = Arc::new(Mutex::new(HashMap::new()));
        let interf = InterfaceBuilder::new(device).finalize();
        let (notify, rx) = mpsc::unbounded();
        let fut = run(interf, socketset.clone(), sources.clone(), rx);

        (
            Net {
                socketset,
                sources,
                notify,
            },
            fut,
        )
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
