#[cfg(feature = "tokio_crate")]
extern crate tokio_crate as tokio;

pub mod device;
mod reactor;
mod socket;
mod socketset;
pub mod util;

use std::sync::Arc;

use device::FutureDevice;
use futures::Future;
use reactor::Reactor;
use socket::{TcpListener, TcpSocket};

use self::socketset::SocketSet;

pub struct Net {
    reactor: Arc<Reactor>,
}

impl Net {
    pub fn new<S: device::Interface + 'static>(
        device: FutureDevice<S>,
    ) -> (Net, impl Future<Output = ()> + Send) {
        let (reactor, fut) = Reactor::new(device);

        (
            Net {
                reactor: Arc::new(reactor),
            },
            fut,
        )
    }
    pub async fn tcp_bind(&self) {
        TcpListener::new(self.reactor.clone()).await;
    }
}
