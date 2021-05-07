#[cfg(feature = "tokio_crate")]
extern crate tokio_crate as tokio;

pub mod device;
mod reactor;
mod socket;
mod socketset;
pub mod util;

use self::socketset::SocketSet;
use device::FutureDevice;
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
