#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

const SIGNAL_HANDLER_FAILED: &str = "failed to install signal handler";

#[cfg(not(unix))]
pub(crate) async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.expect(SIGNAL_HANDLER_FAILED)
}

#[cfg(unix)]
pub(crate) async fn shutdown_signal() {
    let mut terminate = signal(SignalKind::terminate()).expect(SIGNAL_HANDLER_FAILED);
    let mut interrupt = signal(SignalKind::interrupt()).expect(SIGNAL_HANDLER_FAILED);
    tokio::select!(_ = terminate.recv() => (), _ = interrupt.recv() => ())
}
