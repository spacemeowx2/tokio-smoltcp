use futures::{Sink, Stream, StreamExt};
use smoltcp::{
    phy::{Device, DeviceCapabilities, RxToken, TxToken},
    time::Instant,
};
use std::{io, marker::PhantomData};

pub type Packet = Vec<u8>;
pub trait Interface: Stream<Item = Packet> + Sink<Packet, Error = io::Error> + Unpin {}
impl<T> Interface for T where T: Stream<Item = Packet> + Sink<Packet, Error = io::Error> + Unpin {}

pub struct FutureDevice<'a, S> {
    caps: DeviceCapabilities,
    stream: S,
    temp: Option<Packet>,
    _p: PhantomData<&'a ()>,
}

pub struct FutureRxToken(Packet);

impl RxToken for FutureRxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        let p = &mut self.0;
        let result = f(p);
        result
    }
}

pub struct FutureTxToken<'a, S>(&'a mut FutureDevice<'a, S>);

impl<'d, S> TxToken for FutureTxToken<'d, S>
where
    S: Interface,
{
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);

        result
    }
}

impl<'a, S> Device<'a> for FutureDevice<'a, S>
where
    S: Interface,
    S: 'a,
{
    type RxToken = FutureRxToken;
    type TxToken = FutureTxToken<'a, S>;

    fn receive(&'a mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        self.get_next()
            .map(move |p| (FutureRxToken(p), FutureTxToken(self)))
    }

    fn transmit(&'a mut self) -> Option<Self::TxToken> {
        Some(FutureTxToken(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.caps.clone()
    }
}

impl<'a, S> FutureDevice<'a, S>
where
    S: Interface,
    S: 'a,
{
    pub fn new(stream: S, mtu: usize) -> FutureDevice<'a, S> {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = mtu;
        FutureDevice {
            caps,
            stream,
            temp: None,
            _p: PhantomData,
        }
    }
    pub(crate) fn need_wait(&self) -> bool {
        self.temp.is_none()
    }
    pub(crate) async fn wait(&mut self) {
        self.temp = self.stream.next().await;
    }
    fn get_next(&mut self) -> Option<Packet> {
        if let Some(t) = self.temp.take() {
            return Some(t);
        }
        None
    }
}
