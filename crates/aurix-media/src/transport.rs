// This module is kept for potential future use (e.g., WebRTC transport).
// The primary UDP receive loop is in sfu.rs.
// This provides the bind-and-run abstraction.

use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{error, info, warn};

use crate::router::PacketRouter;

pub struct UdpTransport {
    socket: Arc<UdpSocket>,
    router: Arc<PacketRouter>,
}

impl UdpTransport {
    pub async fn bind(addr: &str, router: Arc<PacketRouter>) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = socket.as_raw_fd();
            let buf_size: libc::c_int = 4 * 1024 * 1024;
            unsafe {
                libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t);
                libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t);
            }
        }

        info!("UDP transport bound to {}", addr);

        Ok(Self { socket: Arc::new(socket), router })
    }

    pub fn socket(&self) -> Arc<UdpSocket> {
        self.socket.clone()
    }

    pub async fn run(&self) {
        let mut buf = vec![0u8; 2048];
        loop {
            match self.socket.recv_from(&mut buf).await {
                Ok((len, src_addr)) => {
                    if len < aurix_common::protocol::HEADER_SIZE {
                        continue;
                    }
                    if let Err(e) = self.router.route_packet(&buf[..len], src_addr).await {
                        warn!("Packet routing error from {}: {}", src_addr, e);
                    }
                }
                Err(e) => {
                    error!("UDP recv error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
    }
}