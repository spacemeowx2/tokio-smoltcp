use crate::device::{self, FutureDevice};
use crate::socket_alloctor::SocketAlloctor;
use futures::{future::select, pin_mut};
use smoltcp::{
    iface::Interface,
    time::{Duration, Instant},
};
use std::{future::Future, sync::Arc};
use tokio::sync::Notify;

pub struct Reactor {
    socket_alloctor: Arc<SocketAlloctor>,
    notify: Arc<Notify>,
}

async fn run<S: device::Interface + 'static>(
    mut interf: Interface<'static, FutureDevice<S>>,
    sockets: Arc<SocketAlloctor>,
    notify: Arc<Notify>,
) {
    let default_timeout = Duration::from_secs(60);

    loop {
        let start = Instant::now();
        let deadline = {
            interf
                .poll_delay(&*sockets.as_mut(), start)
                .unwrap_or(default_timeout)
        };
        let device = interf.device_mut();
        device.send_queue().await.expect("Failed to send queue");

        if device.need_wait() {
            let wait = device.wait(deadline.into());
            let notify = notify.notified();
            pin_mut!(wait);
            pin_mut!(notify);
            select(wait, notify).await;
        }

        let mut set = sockets.as_mut();
        let end = Instant::now();
        match interf.poll(&mut set, end) {
            Ok(true) => (),
            // readiness not changed
            Ok(false) | Err(smoltcp::Error::Dropped) => continue,
            Err(_e) => {
                continue;
            }
        };
    }
}

impl Reactor {
    pub fn new<S: device::Interface + 'static>(
        interf: Interface<'static, FutureDevice<S>>,
    ) -> (Self, impl Future<Output = ()> + Send) {
        let socket_alloctor = Arc::new(SocketAlloctor::new(Default::default()));
        let notify = Arc::new(Notify::new());
        let fut = run(interf, socket_alloctor.clone(), notify.clone());

        (
            Reactor {
                socket_alloctor,
                notify,
            },
            fut,
        )
    }
    pub fn socket_alloctor(&self) -> &Arc<SocketAlloctor> {
        &self.socket_alloctor
    }
    pub fn notify(&self) {
        self.notify.clone().notify_waiters();
    }
}
