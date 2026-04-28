use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use tokio::net::TcpListener;

pub fn build_listener(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;

    // SO_BUSY_POLL (linux-tuning feature, Phase 7): set via setsockopt before bind.
    // Placeholder — wired in Phase 7 when the linux-tuning feature is populated.

    socket.bind(&addr.into())?;
    socket.listen(4096)?;

    let listener = TcpListener::from_std(socket.into())?;
    Ok(listener)
}
