use crate::{
    device::{BufferDevice, Packet},
    socket_allocator::{BufferSize, SocketAlloctor},
};
use futures::{FutureExt, SinkExt, StreamExt, stream::iter};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use smoltcp::{
    iface::{Context, Interface, SocketHandle},
    socket::{AnySocket, Socket},
    time::{Duration, Instant},
};
use std::{collections::VecDeque, future::Future, io, sync::Arc};
use tokio::{pin, select, sync::Notify, time::sleep};

pub(crate) type BufferInterface = Arc<Mutex<Interface>>;
const MAX_BURST_SIZE: usize = 100;

pub(crate) struct Reactor {
    notify: Arc<Notify>,
    iface: BufferInterface,
    socket_allocator: SocketAlloctor,
}

async fn receive(async_iface: &mut impl crate::device::AsyncDevice) -> io::Result<Option<Packet>> {
    match async_iface.next().await {
        Some(packet) => Ok(Some(packet?)),
        None => Ok(None),
    }
}

async fn run(
    mut async_iface: impl crate::device::AsyncDevice,
    iface: BufferInterface,
    mut device: BufferDevice,
    socket_allocator: SocketAlloctor,
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
        let mut stream_ended = false;
        let packets = device.take_send_queue();

        async_iface.send_all(&mut iter(packets).map(Ok)).await?;

        if recv_buf.is_empty() && device.need_wait() {
            let start = Instant::now();
            let deadline = {
                iface
                    .lock()
                    .poll_delay(start, &socket_allocator.sockets().lock())
                    .unwrap_or(default_timeout)
            };

            timer
                .as_mut()
                .reset(tokio::time::Instant::now() + deadline.into());
            select! {
                _ = &mut timer => {},
                packet = receive(&mut async_iface) => match packet? {
                    Some(packet) => recv_buf.push_back(packet),
                    None => break,
                },
                _ = notify.notified() => {},
                _ = stopper.notified() => break,
            };

            while recv_buf.len() < max_burst_size {
                match async_iface.next().now_or_never() {
                    Some(Some(Ok(packet))) => recv_buf.push_back(packet),
                    Some(Some(Err(err))) => return Err(err),
                    Some(None) => {
                        stream_ended = true;
                        break;
                    }
                    None => break,
                }
            }
        }

        let mut iface = iface.lock();

        device.push_recv_queue(recv_buf.drain(..device.avaliable_recv_queue().min(recv_buf.len())));

        iface.poll(
            Instant::now(),
            &mut device,
            &mut socket_allocator.sockets().lock(),
        );

        if stream_ended {
            break;
        }
    }

    Ok(())
}

impl Reactor {
    pub fn new(
        async_device: impl crate::device::AsyncDevice,
        iface: Interface,
        device: BufferDevice,
        buffer_size: BufferSize,
        stopper: Arc<Notify>,
    ) -> (Self, impl Future<Output = io::Result<()>> + Send) {
        let iface = Arc::new(Mutex::new(iface));
        let notify = Arc::new(Notify::new());
        let socket_allocator = SocketAlloctor::new(buffer_size);
        let fut = run(
            async_device,
            iface.clone(),
            device,
            socket_allocator.clone(),
            notify.clone(),
            stopper,
        );

        (
            Reactor {
                notify,
                iface: iface.clone(),
                socket_allocator,
            },
            fut,
        )
    }
    pub fn get_socket<T: AnySocket<'static>>(
        &self,
        handle: SocketHandle,
    ) -> MappedMutexGuard<'_, T> {
        MutexGuard::map(
            self.socket_allocator.sockets().lock(),
            |sockets: &mut smoltcp::iface::SocketSet<'_>| sockets.get_mut::<T>(handle),
        )
    }
    pub fn context(&self) -> MappedMutexGuard<'_, Context> {
        MutexGuard::map(self.iface.lock(), |iface| iface.context())
    }
    pub fn socket_allocator(&self) -> &SocketAlloctor {
        &self.socket_allocator
    }
    pub fn notify(&self) {
        self.notify.notify_one();
    }
    pub fn iface(&self) -> &BufferInterface {
        &self.iface
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        for (_, socket) in self.socket_allocator.sockets().lock().iter_mut() {
            match socket {
                Socket::Tcp(tcp) => tcp.close(),
                Socket::Raw(_) => {}
                Socket::Udp(udp) => udp.close(),
                #[allow(unreachable_patterns)]
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::{
        iface::Config,
        phy::{DeviceCapabilities, Medium},
        wire::HardwareAddress,
    };

    #[test]
    fn notification_is_retained_until_reactor_waits() {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = 1500;
        let mut device = BufferDevice::new(caps);
        let iface = Interface::new(
            Config::new(HardwareAddress::Ip),
            &mut device,
            Instant::now(),
        );
        let reactor = Reactor {
            notify: Arc::new(Notify::new()),
            iface: Arc::new(Mutex::new(iface)),
            socket_allocator: SocketAlloctor::new(BufferSize::default()),
        };

        reactor.notify();
        assert!(reactor.notify.notified().now_or_never().is_some());
    }
}
