use crate::{
    device::{BufferDevice, Packet},
    socket_allocator::BufferSize,
};
use futures::{stream::iter, FutureExt, SinkExt, StreamExt};
use parking_lot::Mutex;
use smoltcp::{
    iface::{Interface, SocketHandle as InnerSocketHandle, SocketSet},
    socket::{raw, tcp, udp},
    time::{Duration, Instant},
    wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol, IpVersion},
};
use std::{collections::{HashMap, VecDeque}, future::Future, io, net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr}, sync::{Arc, atomic::{AtomicUsize, Ordering}}};
use tokio::{
    pin, select,
    sync::{mpsc, oneshot, Notify},
    time::sleep,
};

const MAX_BURST_SIZE: usize = 100;

pub(crate) type BufferInterface = Arc<Mutex<Interface>>;
pub(crate) type SocketId = usize;

#[derive(Clone)]
pub(crate) struct Reactor {
    iface: BufferInterface,
    commands: mpsc::UnboundedSender<Command>,
    terminal: Arc<Mutex<TerminalState>>,
    next_id: Arc<AtomicUsize>,
}

#[derive(Default)]
struct TerminalState {
    stopped: bool,
    error: Option<Arc<io::Error>>,
}

struct Entry {
    handle: InnerSocketHandle,
    kind: EntryKind,
}

enum EntryKind {
    TcpListener(TcpListenerEntry),
    TcpStream(TcpStreamEntry),
    Udp(UdpEntry),
    Raw(RawEntry),
}

struct TcpListenerEntry {
    local_addr: std::net::SocketAddr,
    listen_endpoint: IpListenEndpoint,
    accept_waiter: Option<oneshot::Sender<io::Result<AcceptedTcpStream>>>,
}

struct TcpStreamEntry {
    connect_waiter: Option<oneshot::Sender<io::Result<()>>>,
    read_waiter: Option<ReadWaiter>,
    write_waiter: Option<WriteWaiter>,
    flush_waiter: Option<oneshot::Sender<io::Result<()>>>,
    shutdown_waiter: Option<oneshot::Sender<io::Result<()>>>,
}

struct UdpEntry {
    send_waiter: Option<UdpSendWaiter>,
    recv_waiter: Option<UdpRecvWaiter>,
}

struct RawEntry {
    send_waiter: Option<RawSendWaiter>,
    recv_waiter: Option<RawRecvWaiter>,
}

struct ReadWaiter {
    len: usize,
    response: oneshot::Sender<io::Result<Vec<u8>>>,
}

struct WriteWaiter {
    data: Vec<u8>,
    response: oneshot::Sender<io::Result<usize>>,
}

struct UdpSendWaiter {
    data: Vec<u8>,
    target: std::net::SocketAddr,
    response: oneshot::Sender<io::Result<usize>>,
}

struct UdpRecvWaiter {
    len: usize,
    response: oneshot::Sender<io::Result<(Vec<u8>, std::net::SocketAddr)>>,
}

struct RawSendWaiter {
    data: Vec<u8>,
    response: oneshot::Sender<io::Result<usize>>,
}

struct RawRecvWaiter {
    len: usize,
    response: oneshot::Sender<io::Result<Vec<u8>>>,
}

pub(crate) struct AcceptedTcpStream {
    pub(crate) socket_id: SocketId,
    pub(crate) local_addr: std::net::SocketAddr,
    pub(crate) peer_addr: std::net::SocketAddr,
}

pub(crate) struct CreatedTcpStream {
    pub(crate) socket_id: SocketId,
    pub(crate) connect: oneshot::Receiver<io::Result<()>>,
}

pub(crate) struct CreatedSocket {
    pub(crate) socket_id: SocketId,
}

enum Command {
    CreateTcpListener {
        socket_id: SocketId,
        local_endpoint: IpListenEndpoint,
        local_addr: std::net::SocketAddr,
        response: oneshot::Sender<io::Result<CreatedSocket>>,
    },
    CreateTcpStream {
        socket_id: SocketId,
        local_endpoint: IpEndpoint,
        remote_endpoint: IpEndpoint,
        connected: oneshot::Sender<io::Result<()>>,
        response: oneshot::Sender<io::Result<CreatedTcpStream>>,
    },
    CreateUdpSocket {
        socket_id: SocketId,
        local_endpoint: IpListenEndpoint,
        response: oneshot::Sender<io::Result<CreatedSocket>>,
    },
    CreateRawSocket {
        socket_id: SocketId,
        ip_version: IpVersion,
        ip_protocol: IpProtocol,
        response: oneshot::Sender<io::Result<CreatedSocket>>,
    },
    Accept {
        socket_id: SocketId,
        response: oneshot::Sender<io::Result<AcceptedTcpStream>>,
    },
    TcpRead {
        socket_id: SocketId,
        len: usize,
        response: oneshot::Sender<io::Result<Vec<u8>>>,
    },
    TcpWrite {
        socket_id: SocketId,
        data: Vec<u8>,
        response: oneshot::Sender<io::Result<usize>>,
    },
    TcpFlush {
        socket_id: SocketId,
        response: oneshot::Sender<io::Result<()>>,
    },
    TcpShutdown {
        socket_id: SocketId,
        response: oneshot::Sender<io::Result<()>>,
    },
    UdpSend {
        socket_id: SocketId,
        data: Vec<u8>,
        target: std::net::SocketAddr,
        response: oneshot::Sender<io::Result<usize>>,
    },
    UdpRecv {
        socket_id: SocketId,
        len: usize,
        response: oneshot::Sender<io::Result<(Vec<u8>, std::net::SocketAddr)>>,
    },
    RawSend {
        socket_id: SocketId,
        data: Vec<u8>,
        response: oneshot::Sender<io::Result<usize>>,
    },
    RawRecv {
        socket_id: SocketId,
        len: usize,
        response: oneshot::Sender<io::Result<Vec<u8>>>,
    },
    DropSocket {
        socket_id: SocketId,
    },
    #[cfg(test)]
    CloseFirstTcpSocket {
        response: oneshot::Sender<bool>,
    },
}

struct ReactorCore {
    sockets: SocketSet<'static>,
    entries: HashMap<SocketId, Entry>,
    buffer_size: BufferSize,
    next_id: SocketId,
}

fn alloc_tcp_socket(buffer_size: BufferSize) -> tcp::Socket<'static> {
    let rx_buffer = tcp::SocketBuffer::new(vec![0; buffer_size.tcp_rx_size]);
    let tx_buffer = tcp::SocketBuffer::new(vec![0; buffer_size.tcp_tx_size]);
    tcp::Socket::new(rx_buffer, tx_buffer)
}

fn alloc_udp_socket(buffer_size: BufferSize) -> udp::Socket<'static> {
    let rx_buffer = udp::PacketBuffer::new(
        vec![udp::PacketMetadata::EMPTY; buffer_size.udp_rx_meta_size],
        vec![0; buffer_size.udp_rx_size],
    );
    let tx_buffer = udp::PacketBuffer::new(
        vec![udp::PacketMetadata::EMPTY; buffer_size.udp_tx_meta_size],
        vec![0; buffer_size.udp_tx_size],
    );
    udp::Socket::new(rx_buffer, tx_buffer)
}

fn alloc_raw_socket(
    buffer_size: BufferSize,
    ip_version: IpVersion,
    ip_protocol: IpProtocol,
) -> raw::Socket<'static> {
    let rx_buffer = raw::PacketBuffer::new(
        vec![raw::PacketMetadata::EMPTY; buffer_size.raw_rx_meta_size],
        vec![0; buffer_size.raw_rx_size],
    );
    let tx_buffer = raw::PacketBuffer::new(
        vec![raw::PacketMetadata::EMPTY; buffer_size.raw_tx_meta_size],
        vec![0; buffer_size.raw_tx_size],
    );
    raw::Socket::new(ip_version, ip_protocol, rx_buffer, tx_buffer)
}

fn map_send_error(error: mpsc::error::SendError<Command>) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, error.to_string())
}

fn clone_io_error(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

fn closed_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "network reactor stopped")
}

fn ep2sa(ep: &IpEndpoint) -> SocketAddr {
    match ep.addr {
        IpAddress::Ipv4(v4) => SocketAddr::new(IpAddr::V4(Ipv4Addr::from(v4)), ep.port),
        IpAddress::Ipv6(v6) => SocketAddr::new(IpAddr::V6(Ipv6Addr::from(v6)), ep.port),
        #[allow(unreachable_patterns)]
        _ => unreachable!(),
    }
}

fn send_terminal_error<T>(response: oneshot::Sender<io::Result<T>>, error: &io::Error) {
    let _ = response.send(Err(clone_io_error(error)));
}

fn create_tcp_stream_entry(
    connect_waiter: Option<oneshot::Sender<io::Result<()>>>,
) -> TcpStreamEntry {
    TcpStreamEntry {
        connect_waiter,
        read_waiter: None,
        write_waiter: None,
        flush_waiter: None,
        shutdown_waiter: None,
    }
}

fn entry_not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "socket entry not found")
}

fn invalid_state() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "socket operation already pending")
}

async fn receive(async_iface: &mut impl crate::device::AsyncDevice) -> io::Result<Option<Packet>> {
    match async_iface.next().await {
        Some(packet) => Ok(Some(packet?)),
        None => Ok(None),
    }
}

fn remove_socket(core: &mut ReactorCore, socket_id: SocketId) {
    if let Some(entry) = core.entries.remove(&socket_id) {
        let _ = core.sockets.remove(entry.handle);
    }
}

fn process_command(core: &mut ReactorCore, iface: &BufferInterface, command: Command) {
    match command {
        Command::CreateTcpListener {
            socket_id,
            local_endpoint,
            local_addr,
            response,
        } => {
            core.next_id = core.next_id.max(socket_id.saturating_add(1));
            let handle = core.sockets.add(alloc_tcp_socket(core.buffer_size));
            match core.sockets.get_mut::<tcp::Socket>(handle).listen(local_endpoint) {
                Ok(()) => {
                    core.entries.insert(
                        socket_id,
                        Entry {
                            handle,
                            kind: EntryKind::TcpListener(TcpListenerEntry {
                                local_addr,
                                listen_endpoint: local_endpoint,
                                accept_waiter: None,
                            }),
                        },
                    );
                    let _ = response.send(Ok(CreatedSocket { socket_id }));
                }
                Err(error) => {
                    let _ = core.sockets.remove(handle);
                    let _ = response.send(Err(io::Error::other(error.to_string())));
                }
            }
        }
        Command::CreateTcpStream {
            socket_id,
            local_endpoint,
            remote_endpoint,
            connected,
            response,
        } => {
            core.next_id = core.next_id.max(socket_id.saturating_add(1));
            let handle = core.sockets.add(alloc_tcp_socket(core.buffer_size));
            let connect_result = {
                let mut iface = iface.lock();
                let mut context = iface.context();
                core.sockets
                    .get_mut::<tcp::Socket>(handle)
                    .connect(&mut context, remote_endpoint, local_endpoint)
            };

            match connect_result {
                Ok(()) => {
                    core.entries.insert(
                        socket_id,
                        Entry {
                            handle,
                            kind: EntryKind::TcpStream(create_tcp_stream_entry(
                                Some(connected),
                            )),
                        },
                    );
                    let _ = response.send(Ok(CreatedTcpStream {
                        socket_id,
                        connect: oneshot::channel::<io::Result<()>>().1,
                    }));
                }
                Err(error) => {
                    let _ = core.sockets.remove(handle);
                    let _ = connected.send(Err(io::Error::other(error.to_string())));
                    let _ = response.send(Err(io::Error::other(error.to_string())));
                }
            }
        }
        Command::CreateUdpSocket {
            socket_id,
            local_endpoint,
            response,
        } => {
            core.next_id = core.next_id.max(socket_id.saturating_add(1));
            let handle = core.sockets.add(alloc_udp_socket(core.buffer_size));
            match core.sockets.get_mut::<udp::Socket>(handle).bind(local_endpoint) {
                Ok(()) => {
                    core.entries.insert(
                        socket_id,
                        Entry {
                            handle,
                            kind: EntryKind::Udp(UdpEntry {
                                send_waiter: None,
                                recv_waiter: None,
                            }),
                        },
                    );
                    let _ = response.send(Ok(CreatedSocket { socket_id }));
                }
                Err(error) => {
                    let _ = core.sockets.remove(handle);
                    let _ = response.send(Err(io::Error::other(error.to_string())));
                }
            }
        }
        Command::CreateRawSocket {
            socket_id,
            ip_version,
            ip_protocol,
            response,
        } => {
            core.next_id = core.next_id.max(socket_id.saturating_add(1));
            let handle = core
                .sockets
                .add(alloc_raw_socket(core.buffer_size, ip_version, ip_protocol));
            core.entries.insert(
                socket_id,
                Entry {
                    handle,
                    kind: EntryKind::Raw(RawEntry {
                        send_waiter: None,
                        recv_waiter: None,
                    }),
                },
            );
            let _ = response.send(Ok(CreatedSocket { socket_id }));
        }
        Command::Accept { socket_id, response } => match core.entries.get_mut(&socket_id) {
            Some(Entry {
                kind: EntryKind::TcpListener(listener),
                ..
            }) => {
                if listener.accept_waiter.is_some() {
                    let _ = response.send(Err(invalid_state()));
                } else {
                    listener.accept_waiter = Some(response);
                }
            }
            _ => {
                let _ = response.send(Err(entry_not_found()));
            }
        },
        Command::TcpRead {
            socket_id,
            len,
            response,
        } => match core.entries.get_mut(&socket_id) {
            Some(Entry {
                kind: EntryKind::TcpStream(stream),
                ..
            }) => {
                if stream.read_waiter.is_some() {
                    let _ = response.send(Err(invalid_state()));
                } else {
                    stream.read_waiter = Some(ReadWaiter { len, response });
                }
            }
            _ => {
                let _ = response.send(Err(entry_not_found()));
            }
        },
        Command::TcpWrite {
            socket_id,
            data,
            response,
        } => match core.entries.get_mut(&socket_id) {
            Some(Entry {
                kind: EntryKind::TcpStream(stream),
                ..
            }) => {
                if stream.write_waiter.is_some() {
                    let _ = response.send(Err(invalid_state()));
                } else {
                    stream.write_waiter = Some(WriteWaiter { data, response });
                }
            }
            _ => {
                let _ = response.send(Err(entry_not_found()));
            }
        },
        Command::TcpFlush { socket_id, response } => match core.entries.get_mut(&socket_id) {
            Some(Entry {
                kind: EntryKind::TcpStream(stream),
                ..
            }) => {
                if stream.flush_waiter.is_some() {
                    let _ = response.send(Err(invalid_state()));
                } else {
                    stream.flush_waiter = Some(response);
                }
            }
            _ => {
                let _ = response.send(Err(entry_not_found()));
            }
        },
        Command::TcpShutdown { socket_id, response } => match core.entries.get_mut(&socket_id) {
            Some(Entry {
                kind: EntryKind::TcpStream(stream),
                ..
            }) => {
                if stream.shutdown_waiter.is_some() {
                    let _ = response.send(Err(invalid_state()));
                } else {
                    stream.shutdown_waiter = Some(response);
                }
            }
            _ => {
                let _ = response.send(Err(entry_not_found()));
            }
        },
        Command::UdpSend {
            socket_id,
            data,
            target,
            response,
        } => match core.entries.get_mut(&socket_id) {
            Some(Entry {
                kind: EntryKind::Udp(udp),
                ..
            }) => {
                if udp.send_waiter.is_some() {
                    let _ = response.send(Err(invalid_state()));
                } else {
                    udp.send_waiter = Some(UdpSendWaiter {
                        data,
                        target,
                        response,
                    });
                }
            }
            _ => {
                let _ = response.send(Err(entry_not_found()));
            }
        },
        Command::UdpRecv {
            socket_id,
            len,
            response,
        } => match core.entries.get_mut(&socket_id) {
            Some(Entry {
                kind: EntryKind::Udp(udp),
                ..
            }) => {
                if udp.recv_waiter.is_some() {
                    let _ = response.send(Err(invalid_state()));
                } else {
                    udp.recv_waiter = Some(UdpRecvWaiter { len, response });
                }
            }
            _ => {
                let _ = response.send(Err(entry_not_found()));
            }
        },
        Command::RawSend {
            socket_id,
            data,
            response,
        } => match core.entries.get_mut(&socket_id) {
            Some(Entry {
                kind: EntryKind::Raw(raw),
                ..
            }) => {
                if raw.send_waiter.is_some() {
                    let _ = response.send(Err(invalid_state()));
                } else {
                    raw.send_waiter = Some(RawSendWaiter { data, response });
                }
            }
            _ => {
                let _ = response.send(Err(entry_not_found()));
            }
        },
        Command::RawRecv {
            socket_id,
            len,
            response,
        } => match core.entries.get_mut(&socket_id) {
            Some(Entry {
                kind: EntryKind::Raw(raw),
                ..
            }) => {
                if raw.recv_waiter.is_some() {
                    let _ = response.send(Err(invalid_state()));
                } else {
                    raw.recv_waiter = Some(RawRecvWaiter { len, response });
                }
            }
            _ => {
                let _ = response.send(Err(entry_not_found()));
            }
        },
        Command::DropSocket { socket_id } => remove_socket(core, socket_id),
        #[cfg(test)]
        Command::CloseFirstTcpSocket { response } => {
            let mut closed = false;
            for entry in core.entries.values() {
                if matches!(entry.kind, EntryKind::TcpStream(_)) {
                    core.sockets.get_mut::<tcp::Socket>(entry.handle).close();
                    closed = true;
                    break;
                }
            }
            let _ = response.send(closed);
        }
    }
}

fn process_commands(
    core: &mut ReactorCore,
    iface: &BufferInterface,
    command: Command,
    command_rx: &mut mpsc::UnboundedReceiver<Command>,
) {
    process_command(core, iface, command);
    while let Ok(command) = command_rx.try_recv() {
        process_command(core, iface, command);
    }
}

fn process_entries(core: &mut ReactorCore) {
    let ids: Vec<SocketId> = core.entries.keys().copied().collect();
    let mut pending_accepts = Vec::new();

    for socket_id in ids {
        let Some(entry) = core.entries.get_mut(&socket_id) else {
            continue;
        };

        match &mut entry.kind {
            EntryKind::TcpListener(listener) => {
                let socket = core.sockets.get_mut::<tcp::Socket>(entry.handle);
                if socket.state() == tcp::State::Established {
                    if let Some(response) = listener.accept_waiter.take() {
                        let peer_addr = socket.remote_endpoint().map(|ep| ep2sa(&ep));
                        let local_addr = socket.local_endpoint().map(|ep| ep2sa(&ep));
                        if let (Some(peer_addr), Some(local_addr)) = (peer_addr, local_addr) {
                            pending_accepts.push((
                                socket_id,
                                listener.listen_endpoint,
                                listener.local_addr,
                                entry.handle,
                                response,
                                local_addr,
                                peer_addr,
                            ));
                        } else {
                            let _ = response.send(Err(io::Error::new(
                                io::ErrorKind::ConnectionAborted,
                                "tcp listener lost connection state",
                            )));
                        }
                    }
                }
            }
            EntryKind::TcpStream(stream) => {
                let socket = core.sockets.get_mut::<tcp::Socket>(entry.handle);

                if let Some(response) = stream.connect_waiter.take() {
                    let result = if socket.state() == tcp::State::Established {
                        Ok(())
                    } else if socket.state() == tcp::State::Closed {
                        Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "tcp connect failed",
                        ))
                    } else {
                        stream.connect_waiter = Some(response);
                        continue;
                    };
                    let _ = response.send(result);
                }

                if let Some(waiter) = stream.read_waiter.take() {
                    let result = if !socket.may_recv() {
                        Ok(Vec::new())
                    } else if socket.can_recv() {
                        let mut buffer = vec![0; waiter.len];
                        match socket.recv_slice(&mut buffer) {
                            Ok(size) => {
                                buffer.truncate(size);
                                Ok(buffer)
                            }
                            Err(error) => Err(io::Error::other(error.to_string())),
                        }
                    } else {
                        stream.read_waiter = Some(waiter);
                        continue;
                    };
                    let _ = waiter.response.send(result);
                }

                if let Some(waiter) = stream.write_waiter.take() {
                    let result = if !socket.may_send() {
                        Err(io::ErrorKind::BrokenPipe.into())
                    } else if socket.can_send() {
                        socket.send_slice(&waiter.data).map_err(|error| io::Error::other(error.to_string()))
                    } else {
                        stream.write_waiter = Some(waiter);
                        continue;
                    };
                    let _ = waiter.response.send(result);
                }

                if let Some(response) = stream.flush_waiter.take() {
                    if socket.send_queue() == 0 {
                        let _ = response.send(Ok(()));
                    } else {
                        stream.flush_waiter = Some(response);
                    }
                }

                if let Some(response) = stream.shutdown_waiter.take() {
                    if socket.is_open() {
                        socket.close();
                    }
                    if socket.state() == tcp::State::Closed {
                        let _ = response.send(Ok(()));
                    } else {
                        stream.shutdown_waiter = Some(response);
                    }
                }
            }
            EntryKind::Udp(udp) => {
                let socket = core.sockets.get_mut::<udp::Socket>(entry.handle);

                if let Some(waiter) = udp.send_waiter.take() {
                    let target: IpEndpoint = waiter.target.into();
                    match socket.send_slice(&waiter.data, target) {
                        Ok(()) => {
                            let _ = waiter.response.send(Ok(waiter.data.len()));
                        }
                        Err(udp::SendError::BufferFull) => {
                            udp.send_waiter = Some(waiter);
                        }
                        Err(error) => {
                            let _ = waiter.response.send(Err(io::Error::other(error.to_string())));
                        }
                    }
                }

                if let Some(waiter) = udp.recv_waiter.take() {
                    let mut buffer = vec![0; waiter.len];
                    match socket.recv_slice(&mut buffer) {
                        Ok((size, meta)) => {
                            buffer.truncate(size);
                            let _ = waiter.response.send(Ok((buffer, ep2sa(&meta.endpoint))));
                        }
                        Err(udp::RecvError::Exhausted) => {
                            udp.recv_waiter = Some(waiter);
                        }
                        Err(udp::RecvError::Truncated) => {
                            let _ = waiter
                                .response
                                .send(Err(io::Error::new(io::ErrorKind::InvalidData, "udp packet truncated")));
                        }
                    }
                }
            }
            EntryKind::Raw(raw) => {
                let socket = core.sockets.get_mut::<raw::Socket>(entry.handle);

                if let Some(waiter) = raw.send_waiter.take() {
                    match socket.send_slice(&waiter.data) {
                        Ok(()) => {
                            let _ = waiter.response.send(Ok(waiter.data.len()));
                        }
                        Err(raw::SendError::BufferFull) => {
                            raw.send_waiter = Some(waiter);
                        }
                    }
                }

                if let Some(waiter) = raw.recv_waiter.take() {
                    let mut buffer = vec![0; waiter.len];
                    match socket.recv_slice(&mut buffer) {
                        Ok(size) => {
                            buffer.truncate(size);
                            let _ = waiter.response.send(Ok(buffer));
                        }
                        Err(raw::RecvError::Exhausted) => {
                            raw.recv_waiter = Some(waiter);
                        }
                        Err(raw::RecvError::Truncated) => {
                            let _ = waiter
                                .response
                                .send(Err(io::Error::new(io::ErrorKind::InvalidData, "raw packet truncated")));
                        }
                    }
                }
            }
        }
    }

    for (listener_id, listen_endpoint, local_addr, old_handle, response, accepted_local, peer_addr) in pending_accepts {
        let new_listener_handle = core.sockets.add(alloc_tcp_socket(core.buffer_size));
        if let Err(error) = core
            .sockets
            .get_mut::<tcp::Socket>(new_listener_handle)
            .listen(listen_endpoint)
        {
            let _ = response.send(Err(io::Error::other(error.to_string())));
            let _ = core.sockets.remove(new_listener_handle);
            continue;
        }

        if let Some(listener_entry) = core.entries.get_mut(&listener_id) {
            listener_entry.handle = new_listener_handle;
            if let EntryKind::TcpListener(listener) = &mut listener_entry.kind {
                listener.local_addr = local_addr;
            }
        }

        let stream_id = core.next_id;
        core.next_id = core.next_id.saturating_add(1);
        core.entries.insert(
            stream_id,
            Entry {
                handle: old_handle,
                kind: EntryKind::TcpStream(create_tcp_stream_entry(None)),
            },
        );
        let _ = response.send(Ok(AcceptedTcpStream {
            socket_id: stream_id,
            local_addr: accepted_local,
            peer_addr,
        }));
    }
}

fn finish_reactor(core: &mut ReactorCore, terminal: &Arc<Mutex<TerminalState>>, result: &io::Result<()>) {
    let error = result
        .as_ref()
        .err()
        .map(clone_io_error)
        .unwrap_or_else(closed_error);

    for (_, entry) in core.entries.drain() {
        let _ = core.sockets.remove(entry.handle);
        match entry.kind {
            EntryKind::TcpListener(listener) => {
                if let Some(response) = listener.accept_waiter {
                    send_terminal_error(response, &error);
                }
            }
            EntryKind::TcpStream(stream) => {
                if let Some(response) = stream.connect_waiter {
                    send_terminal_error(response, &error);
                }
                if let Some(waiter) = stream.read_waiter {
                    send_terminal_error(waiter.response, &error);
                }
                if let Some(waiter) = stream.write_waiter {
                    send_terminal_error(waiter.response, &error);
                }
                if let Some(response) = stream.flush_waiter {
                    send_terminal_error(response, &error);
                }
                if let Some(response) = stream.shutdown_waiter {
                    send_terminal_error(response, &error);
                }
            }
            EntryKind::Udp(udp) => {
                if let Some(waiter) = udp.send_waiter {
                    send_terminal_error(waiter.response, &error);
                }
                if let Some(waiter) = udp.recv_waiter {
                    send_terminal_error(waiter.response, &error);
                }
            }
            EntryKind::Raw(raw) => {
                if let Some(waiter) = raw.send_waiter {
                    send_terminal_error(waiter.response, &error);
                }
                if let Some(waiter) = raw.recv_waiter {
                    send_terminal_error(waiter.response, &error);
                }
            }
        }
    }

    let mut terminal = terminal.lock();
    terminal.stopped = true;
    terminal.error = result.as_ref().err().map(|error| Arc::new(clone_io_error(error)));
}

async fn run(
    mut async_iface: impl crate::device::AsyncDevice,
    iface: BufferInterface,
    mut device: BufferDevice,
    mut command_rx: mpsc::UnboundedReceiver<Command>,
    stopper: Arc<Notify>,
    buffer_size: BufferSize,
    terminal: Arc<Mutex<TerminalState>>,
) -> io::Result<()> {
    let default_timeout = Duration::from_secs(60);
    let timer = sleep(default_timeout.into());
    let max_burst_size = async_iface
        .capabilities()
        .max_burst_size
        .unwrap_or(MAX_BURST_SIZE);
    let mut recv_buf = VecDeque::with_capacity(max_burst_size);
    let mut core = ReactorCore {
        sockets: SocketSet::new(Vec::new()),
        entries: HashMap::new(),
        buffer_size,
        next_id: 1,
    };
    pin!(timer);

    let result = 'main: loop {
        while let Ok(command) = command_rx.try_recv() {
            process_command(&mut core, &iface, command);
        }

        process_entries(&mut core);

        let packets = device.take_send_queue();
        async_iface.send_all(&mut iter(packets).map(Ok)).await?;

        if recv_buf.is_empty() && device.need_wait() {
            let start = Instant::now();
            let deadline = {
                let mut iface = iface.lock();
                iface.poll_delay(start, &core.sockets).unwrap_or(default_timeout)
            };

            timer
                .as_mut()
                .reset(tokio::time::Instant::now() + deadline.into());

            select! {
                _ = &mut timer => {},
                packet = receive(&mut async_iface) => match packet? {
                    Some(packet) => recv_buf.push_back(packet),
                    None => break Ok(()),
                },
                command = command_rx.recv() => match command {
                    Some(command) => process_commands(&mut core, &iface, command, &mut command_rx),
                    None => break Ok(()),
                },
                _ = stopper.notified() => break Ok(()),
            }

            while recv_buf.len() < max_burst_size {
                let mut stop_result = None;
                match async_iface.next().now_or_never() {
                    Some(Some(Ok(packet))) => recv_buf.push_back(packet),
                    Some(Some(Err(error))) => stop_result = Some(Err(error)),
                    Some(None) => {
                        stop_result = Some(Ok(()));
                    }
                    None => break,
                }
                if let Some(result) = stop_result {
                    break 'main result;
                }
            }
        }

        {
            let mut iface = iface.lock();
            device.push_recv_queue(recv_buf.drain(..device.avaliable_recv_queue().min(recv_buf.len())));
            iface.poll(Instant::now(), &mut device, &mut core.sockets);
        }

        process_entries(&mut core);
    };

    finish_reactor(&mut core, &terminal, &result);
    result
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
        let (commands, command_rx) = mpsc::unbounded_channel();
        let terminal = Arc::new(Mutex::new(TerminalState::default()));
        let fut = run(
            async_device,
            iface.clone(),
            device,
            command_rx,
            stopper,
            buffer_size,
            terminal.clone(),
        );

        (
            Self {
                iface,
                commands,
                terminal,
                next_id: Arc::new(AtomicUsize::new(1)),
            },
            fut,
        )
    }

    fn next_socket_id(&self) -> SocketId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn with_iface<R>(&self, f: impl FnOnce(&mut Interface) -> R) -> R {
        let mut iface = self.iface.lock();
        f(&mut iface)
    }

    pub(crate) fn terminal_error(&self) -> Option<io::Error> {
        let terminal = self.terminal.lock();
        terminal
            .error
            .as_ref()
            .map(|error| clone_io_error(error))
            .or_else(|| terminal.stopped.then(closed_error))
    }

    pub(crate) async fn create_tcp_listener(
        &self,
        local_endpoint: IpListenEndpoint,
        local_addr: std::net::SocketAddr,
    ) -> io::Result<CreatedSocket> {
        let (tx, rx) = oneshot::channel();
        let socket_id = self.next_socket_id();
        self.commands
            .send(Command::CreateTcpListener {
                socket_id,
                local_endpoint,
                local_addr,
                response: tx,
            })
            .map_err(map_send_error)?;
        rx.await.map_err(|_| closed_error())?
    }

    pub(crate) async fn create_tcp_stream(
        &self,
        local_endpoint: IpEndpoint,
        remote_endpoint: IpEndpoint,
    ) -> io::Result<CreatedTcpStream> {
        let (tx, rx) = oneshot::channel();
        let (connected_tx, connected_rx) = oneshot::channel();
        let socket_id = self.next_socket_id();
        self.commands
            .send(Command::CreateTcpStream {
                socket_id,
                local_endpoint,
                remote_endpoint,
                connected: connected_tx,
                response: tx,
            })
            .map_err(map_send_error)?;
        let mut created = rx.await.map_err(|_| closed_error())??;
        created.connect = connected_rx;
        Ok(created)
    }

    pub(crate) async fn create_udp_socket(
        &self,
        local_endpoint: IpListenEndpoint,
    ) -> io::Result<CreatedSocket> {
        let (tx, rx) = oneshot::channel();
        let socket_id = self.next_socket_id();
        self.commands
            .send(Command::CreateUdpSocket {
                socket_id,
                local_endpoint,
                response: tx,
            })
            .map_err(map_send_error)?;
        rx.await.map_err(|_| closed_error())?
    }

    pub(crate) async fn create_raw_socket(
        &self,
        ip_version: IpVersion,
        ip_protocol: IpProtocol,
    ) -> io::Result<CreatedSocket> {
        let (tx, rx) = oneshot::channel();
        let socket_id = self.next_socket_id();
        self.commands
            .send(Command::CreateRawSocket {
                socket_id,
                ip_version,
                ip_protocol,
                response: tx,
            })
            .map_err(map_send_error)?;
        rx.await.map_err(|_| closed_error())?
    }

    pub(crate) fn accept(&self, socket_id: SocketId) -> io::Result<oneshot::Receiver<io::Result<AcceptedTcpStream>>> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Accept { socket_id, response: tx })
            .map_err(map_send_error)?;
        Ok(rx)
    }

    pub(crate) fn tcp_read(&self, socket_id: SocketId, len: usize) -> io::Result<oneshot::Receiver<io::Result<Vec<u8>>>> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::TcpRead {
                socket_id,
                len,
                response: tx,
            })
            .map_err(map_send_error)?;
        Ok(rx)
    }

    pub(crate) fn tcp_write(&self, socket_id: SocketId, data: Vec<u8>) -> io::Result<oneshot::Receiver<io::Result<usize>>> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::TcpWrite {
                socket_id,
                data,
                response: tx,
            })
            .map_err(map_send_error)?;
        Ok(rx)
    }

    pub(crate) fn tcp_flush(&self, socket_id: SocketId) -> io::Result<oneshot::Receiver<io::Result<()>>> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::TcpFlush {
                socket_id,
                response: tx,
            })
            .map_err(map_send_error)?;
        Ok(rx)
    }

    pub(crate) fn tcp_shutdown(&self, socket_id: SocketId) -> io::Result<oneshot::Receiver<io::Result<()>>> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::TcpShutdown {
                socket_id,
                response: tx,
            })
            .map_err(map_send_error)?;
        Ok(rx)
    }

    pub(crate) fn udp_send(
        &self,
        socket_id: SocketId,
        data: Vec<u8>,
        target: std::net::SocketAddr,
    ) -> io::Result<oneshot::Receiver<io::Result<usize>>> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::UdpSend {
                socket_id,
                data,
                target,
                response: tx,
            })
            .map_err(map_send_error)?;
        Ok(rx)
    }

    pub(crate) fn udp_recv(
        &self,
        socket_id: SocketId,
        len: usize,
    ) -> io::Result<oneshot::Receiver<io::Result<(Vec<u8>, std::net::SocketAddr)>>> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::UdpRecv {
                socket_id,
                len,
                response: tx,
            })
            .map_err(map_send_error)?;
        Ok(rx)
    }

    pub(crate) fn raw_send(&self, socket_id: SocketId, data: Vec<u8>) -> io::Result<oneshot::Receiver<io::Result<usize>>> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::RawSend {
                socket_id,
                data,
                response: tx,
            })
            .map_err(map_send_error)?;
        Ok(rx)
    }

    pub(crate) fn raw_recv(&self, socket_id: SocketId, len: usize) -> io::Result<oneshot::Receiver<io::Result<Vec<u8>>>> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::RawRecv {
                socket_id,
                len,
                response: tx,
            })
            .map_err(map_send_error)?;
        Ok(rx)
    }

    pub(crate) fn drop_socket(&self, socket_id: SocketId) {
        let _ = self.commands.send(Command::DropSocket { socket_id });
    }

    #[cfg(test)]
    pub(crate) async fn close_first_tcp_socket_for_test(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .commands
            .send(Command::CloseFirstTcpSocket { response: tx })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}
