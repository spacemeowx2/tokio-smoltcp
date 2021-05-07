#[cfg(feature = "raw_socket")]
use smoltcp::socket::{RawPacketMetadata, RawSocket, RawSocketBuffer};
use smoltcp::{
    socket::{
        self, AnySocket, SocketHandle, SocketRef, SocketSet as InnerSocketSet, TcpSocket,
        TcpSocketBuffer,
    },
    wire::{IpProtocol, IpVersion},
};

#[derive(Debug, Clone, Copy)]
pub struct BufferSize {
    pub tcp_rx_size: usize,
    pub tcp_tx_size: usize,
    pub raw_size: usize,
}

impl Default for BufferSize {
    fn default() -> Self {
        BufferSize {
            tcp_rx_size: 8192,
            tcp_tx_size: 8192,
            raw_size: 8192,
        }
    }
}

pub struct SocketSet {
    buffer_size: BufferSize,
    set: InnerSocketSet<'static>,
}

impl SocketSet {
    pub fn new(buffer_size: BufferSize) -> SocketSet {
        SocketSet {
            buffer_size,
            set: InnerSocketSet::new(vec![]),
        }
    }
    pub fn as_set_mut(&mut self) -> &mut InnerSocketSet<'static> {
        &mut self.set
    }
    pub fn get<T: AnySocket<'static>>(&mut self, handle: SocketHandle) -> SocketRef<T> {
        self.set.get(handle)
    }
    pub fn remove(&mut self, handle: SocketHandle) {
        self.set.remove(handle);
    }
    pub fn new_tcp_socket(&mut self) -> SocketHandle {
        let handle = self.set.add(self.alloc_tcp_socket());
        handle
    }
    pub fn new_raw_socket(&mut self) -> SocketHandle {
        let handle = self.set.add(self.alloc_raw_socket());
        handle
    }
    fn alloc_tcp_socket(&self) -> socket::TcpSocket<'static> {
        let rx_buffer = TcpSocketBuffer::new(vec![0; self.buffer_size.tcp_rx_size]);
        let tx_buffer = TcpSocketBuffer::new(vec![0; self.buffer_size.tcp_tx_size]);
        let mut tcp = TcpSocket::new(rx_buffer, tx_buffer);
        tcp.listen(0).unwrap();

        tcp
    }
    #[cfg(feature = "raw_socket")]
    fn alloc_raw_socket(&self) -> socket::RawSocket<'static> {
        let rx_buffer = RawSocketBuffer::new(
            vec![RawPacketMetadata::EMPTY; 32],
            vec![0; self.buffer_size.raw_size],
        );
        let tx_buffer = RawSocketBuffer::new(
            vec![RawPacketMetadata::EMPTY; 32],
            vec![0; self.buffer_size.raw_size],
        );
        let raw = RawSocket::new(IpVersion::Ipv4, IpProtocol::Udp, rx_buffer, tx_buffer);

        raw
    }
}
