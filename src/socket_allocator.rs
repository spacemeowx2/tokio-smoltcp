use parking_lot::Mutex;
use smoltcp::{
    iface::{SocketHandle as InnerSocketHandle, SocketSet},
    socket::{raw, tcp, udp},
    wire::{IpProtocol, IpVersion},
};
use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

/// `BufferSize` is used to configure the size of the socket buffer.
#[derive(Debug, Clone, Copy)]
pub struct BufferSize {
    pub tcp_rx_size: usize,
    pub tcp_tx_size: usize,
    pub udp_rx_size: usize,
    pub udp_tx_size: usize,
    pub udp_rx_meta_size: usize,
    pub udp_tx_meta_size: usize,
    pub raw_rx_size: usize,
    pub raw_tx_size: usize,
    pub raw_rx_meta_size: usize,
    pub raw_tx_meta_size: usize,
}

impl Default for BufferSize {
    fn default() -> Self {
        BufferSize {
            tcp_rx_size: 8192,
            tcp_tx_size: 8192,
            udp_rx_size: 8192,
            udp_tx_size: 8192,
            udp_rx_meta_size: 32,
            udp_tx_meta_size: 32,
            raw_rx_size: 8192,
            raw_tx_size: 8192,
            raw_rx_meta_size: 32,
            raw_tx_meta_size: 32,
        }
    }
}

type SharedSocketSet = Arc<Mutex<SocketSet<'static>>>;

#[derive(Clone)]
pub struct SocketAlloctor {
    sockets: SharedSocketSet,
    buffer_size: BufferSize,
}

impl SocketAlloctor {
    pub(crate) fn new(buffer_size: BufferSize) -> SocketAlloctor {
        let sockets = Arc::new(Mutex::new(SocketSet::new(Vec::new())));
        SocketAlloctor {
            sockets,
            buffer_size,
        }
    }
    pub(crate) fn sockets(&self) -> &SharedSocketSet {
        &self.sockets
    }
    pub fn new_tcp_socket(&self) -> SocketHandle {
        let mut set = self.sockets.lock();
        let handle = set.add(self.alloc_tcp_socket());
        SocketHandle::new(handle, self.sockets.clone())
    }
    pub fn new_udp_socket(&self) -> SocketHandle {
        let mut set = self.sockets.lock();
        let handle = set.add(self.alloc_udp_socket());
        SocketHandle::new(handle, self.sockets.clone())
    }
    pub fn new_raw_socket(&self, ip_version: IpVersion, ip_protocol: IpProtocol) -> SocketHandle {
        let mut set = self.sockets.lock();
        let handle = set.add(self.alloc_raw_socket(ip_version, ip_protocol));
        SocketHandle::new(handle, self.sockets.clone())
    }
    fn alloc_tcp_socket(&self) -> tcp::Socket<'static> {
        let rx_buffer = tcp::SocketBuffer::new(vec![0; self.buffer_size.tcp_rx_size]);
        let tx_buffer = tcp::SocketBuffer::new(vec![0; self.buffer_size.tcp_tx_size]);
        let tcp = tcp::Socket::new(rx_buffer, tx_buffer);

        tcp
    }
    fn alloc_udp_socket(&self) -> udp::Socket<'static> {
        let rx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; self.buffer_size.udp_rx_meta_size],
            vec![0; self.buffer_size.udp_rx_size],
        );
        let tx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; self.buffer_size.udp_tx_meta_size],
            vec![0; self.buffer_size.udp_tx_size],
        );
        let udp = udp::Socket::new(rx_buffer, tx_buffer);

        udp
    }
    fn alloc_raw_socket(
        &self,
        ip_version: IpVersion,
        ip_protocol: IpProtocol,
    ) -> raw::Socket<'static> {
        let rx_buffer = raw::PacketBuffer::new(
            vec![raw::PacketMetadata::EMPTY; self.buffer_size.raw_rx_meta_size],
            vec![0; self.buffer_size.raw_rx_size],
        );
        let tx_buffer = raw::PacketBuffer::new(
            vec![raw::PacketMetadata::EMPTY; self.buffer_size.raw_tx_meta_size],
            vec![0; self.buffer_size.raw_tx_size],
        );
        let raw = raw::Socket::new(ip_version, ip_protocol, rx_buffer, tx_buffer);

        raw
    }
}

pub struct SocketHandle(InnerSocketHandle, SharedSocketSet);

impl SocketHandle {
    fn new(inner: InnerSocketHandle, set: SharedSocketSet) -> SocketHandle {
        SocketHandle(inner, set)
    }
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        let mut iface = self.1.lock();
        iface.remove(self.0);
    }
}

impl Deref for SocketHandle {
    type Target = InnerSocketHandle;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SocketHandle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
