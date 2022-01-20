use crate::{
    device::{BufferDevice, Packet},
    socket_allocator::{BufferSize, SocketAlloctor},
};
use futures::{stream::iter, FutureExt, SinkExt, StreamExt};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use smoltcp::{
    iface::{Interface, SocketHandle},
    socket::{AnySocket, Socket},
    time::{Duration, Instant},
};
use std::{collections::VecDeque, future::Future, io, sync::Arc};
use tokio::{pin, select, sync::Notify, time::sleep};

pub(crate) type BufferInterface = Arc<Mutex<Interface<'static, BufferDevice>>>;
const MAX_BURST_SIZE: usize = 100;

pub(crate) struct Reactor {
    notify: Arc<Notify>,
    interf: BufferInterface,
    socket_allocator: Arc<SocketAlloctor>,
}

async fn receive(
    async_iface: &mut impl crate::device::AsyncDevice,
    recv_buf: &mut VecDeque<Packet>,
) -> io::Result<()> {
    if let Some(packet) = async_iface.next().await {
        recv_buf.push_back(packet?);
    }
    Ok(())
}

async fn run(
    mut async_iface: impl crate::device::AsyncDevice,
    interf: BufferInterface,
    notify: Arc<Notify>,
    stopper: Arc<Notify>,
) -> io::Result<()> {
    let default_timeout = Duration::from_secs(60);
    let timer = sleep(default_timeout.into());
    let max_burst_size = async_iface
        .capabilities()
        .max_burst_size
        .unwrap_or(MAX_BURST_SIZE);
    let mut recv_buf = VecDeque::with_capacity(max_burst_size);
    pin!(timer);

    loop {
        let packets = interf.lock().device_mut().take_send_queue();

        async_iface
            .send_all(&mut iter(packets).map(|p| Ok(p)))
            .await?;

        if interf.lock().device().need_wait() {
            let start = Instant::now();
            let deadline = { interf.lock().poll_delay(start).unwrap_or(default_timeout) };

            timer
                .as_mut()
                .reset(tokio::time::Instant::now() + deadline.into());
            select! {
                _ = &mut timer => {},
                _ = receive(&mut async_iface,&mut recv_buf) => {}
                _ = notify.notified() => {}
                _ = stopper.notified() => break,
            };

            while let (true, Some(Ok(p))) = (
                recv_buf.len() < max_burst_size,
                async_iface.next().now_or_never().flatten(),
            ) {
                recv_buf.push_back(p);
            }
        }

        let mut interf = interf.lock();
        let dev = interf.device_mut();

        dev.push_recv_queue(recv_buf.drain(..dev.avaliable_recv_queue().min(recv_buf.len())));

        while !matches!(
            interf.poll(Instant::now()),
            Ok(_) | Err(smoltcp::Error::Exhausted)
        ) {}
    }

    Ok(())
}

impl Reactor {
    pub fn new(
        async_device: impl crate::device::AsyncDevice,
        interf: Interface<'static, BufferDevice>,
        buffer_size: BufferSize,
        stopper: Arc<Notify>,
    ) -> (Self, impl Future<Output = io::Result<()>> + Send) {
        let interf = Arc::new(Mutex::new(interf));
        let notify = Arc::new(Notify::new());
        let fut = run(async_device, interf.clone(), notify.clone(), stopper);

        (
            Reactor {
                notify,
                interf: interf.clone(),
                socket_allocator: Arc::new(SocketAlloctor::new(interf, buffer_size)),
            },
            fut,
        )
    }
    pub fn get_socket<T: AnySocket<'static>>(
        &self,
        handle: SocketHandle,
    ) -> MappedMutexGuard<'_, T> {
        MutexGuard::map(self.interf.lock(), |interf| interf.get_socket::<T>(handle))
    }
    pub fn borrow_socket<T: AnySocket<'static>, F, R>(&self, handle: SocketHandle, cb: F) -> R
    where
        F: FnOnce(&mut T, &mut smoltcp::iface::Context) -> R,
    {
        let mut interf = self.interf.lock();
        let (socket, ctx) = interf.get_socket_and_context::<T>(handle);
        cb(socket, ctx)
    }
    pub fn socket_allocator(&self) -> &Arc<SocketAlloctor> {
        &self.socket_allocator
    }
    pub fn notify(&self) {
        self.notify.notify_waiters();
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        for (_, socket) in self.interf.lock().sockets_mut() {
            match socket {
                Socket::Tcp(tcp) => tcp.close(),
                Socket::Raw(_) => {}
                Socket::Udp(udp) => udp.close(),
            }
        }
    }
}
