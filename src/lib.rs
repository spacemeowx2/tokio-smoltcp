#[cfg(feature = "tokio_crate")]
extern crate tokio_crate as tokio;

pub mod device;
pub mod util;

use device::{FutureDevice, Interface};

pub async fn run<S: Interface>(device: FutureDevice<'static, S>) {}
