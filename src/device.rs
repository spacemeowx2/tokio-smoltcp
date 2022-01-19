use futures::{Sink, Stream};
use smoltcp::{
    phy::{Device, DeviceCapabilities, RxToken, TxToken},
    time::Instant,
};
use std::{collections::VecDeque, io};

pub const DEFAULT_MAX_BURST_SIZE: usize = 100;

pub type Packet = Vec<u8>;
pub trait AsyncDevice:
    Stream<Item = io::Result<Packet>> + Sink<Packet, Error = io::Error> + Send + Unpin
{
    fn capabilities(&self) -> &DeviceCapabilities;
}

pub(crate) struct BufferDevice {
    caps: DeviceCapabilities,
    max_burst_size: usize,
    recv_queue: VecDeque<Packet>,
    send_queue: VecDeque<Packet>,
}

pub struct BufferRxToken(Packet);

impl RxToken for BufferRxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        let p = &mut self.0;
        let result = f(p);
        result
    }
}

pub struct BufferTxToken<'a>(&'a mut BufferDevice);

impl<'d> TxToken for BufferTxToken<'d> {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);

        if result.is_ok() {
            self.0.send_queue.push_back(buffer);
        }

        result
    }
}

impl<'a> Device<'a> for BufferDevice {
    type RxToken = BufferRxToken;
    type TxToken = BufferTxToken<'a>;

    fn receive(&'a mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        match self.recv_queue.pop_front() {
            Some(p) => Some((BufferRxToken(p), BufferTxToken(self))),
            None => None,
        }
    }

    fn transmit(&'a mut self) -> Option<Self::TxToken> {
        if self.send_queue.len() < self.max_burst_size {
            Some(BufferTxToken(self))
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.caps.clone()
    }
}

impl BufferDevice {
    pub fn new(caps: DeviceCapabilities) -> BufferDevice {
        let max_burst_size = caps.max_burst_size.unwrap_or(DEFAULT_MAX_BURST_SIZE);
        BufferDevice {
            caps,
            max_burst_size,
            recv_queue: VecDeque::with_capacity(max_burst_size),
            send_queue: VecDeque::with_capacity(max_burst_size),
        }
    }
    pub fn take_send_queue(&mut self) -> VecDeque<Packet> {
        std::mem::replace(
            &mut self.send_queue,
            VecDeque::with_capacity(self.max_burst_size),
        )
    }
    pub fn push_recv_queue(&mut self, p: impl Iterator<Item = Packet>) {
        self.recv_queue.extend(p.take(self.avaliable_recv_queue()));
    }
    pub fn avaliable_recv_queue(&self) -> usize {
        self.max_burst_size - self.recv_queue.len()
    }
    pub fn need_wait(&self) -> bool {
        self.recv_queue.len() == 0
    }
}
