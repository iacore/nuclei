use std::io;
use std::io::{Read, Write};
use std::{fs::File, os::unix::io::{AsRawFd, FromRawFd}, mem::ManuallyDrop};
use std::net::{SocketAddr, ToSocketAddrs, TcpListener};
use std::os::unix::net::{
    SocketAddr as UnixSocketAddr, UnixDatagram, UnixListener, UnixStream,
};
use std::net::{SocketAddrV6, SocketAddrV4, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::future::Future;
use std::path::Path;
use std::net::TcpStream;

use crate::proactor::Proactor;

use crate::Handle;
use crate::syscore::shim_to_af_unix;
use std::io::{IoSliceMut, IoSlice};
use iou::{SockFlag, SockAddrStorage};
use std::mem::MaybeUninit;



pub struct Processor;

impl Processor {
    ///////////////////////////////////
    ///// Read Write
    ///// Synchronous File
    ///////////////////////////////////

    pub(crate) async fn processor_read_file<R: AsRawFd>(io: &R, buf: &mut [u8]) -> io::Result<usize> {
        let fd = io.as_raw_fd() as _;

        let cc = Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_read_vectored(fd, &mut [IoSliceMut::new(buf)], 0);
        })?;

        Ok(cc.await? as _)
    }

    pub(crate) async fn processor_write_file<R: AsRawFd>(io: &R, buf: &[u8]) -> io::Result<usize> {
        let fd = io.as_raw_fd() as _;

        let cc = Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_write_vectored(fd, &mut [IoSlice::new(buf)], 0);
        })?;

        Ok(cc.await? as _)
    }

    ///////////////////////////////////
    ///// Send, Recv, Peek
    ///// Commonality of TcpStream, UdpSocket, UnixStream, UnixDatagram
    ///////////////////////////////////

    pub(crate) async fn processor_send<R: AsRawFd>(socket: &R, buf: &[u8]) -> io::Result<usize> {
        let fd = socket.as_raw_fd() as _;

        let res = Proactor::get().inner().register_io(|sqe| unsafe {
            let sqep = sqe.raw_mut();
            uring_sys::io_uring_prep_send(sqep, fd, buf.as_ptr() as _, buf.len() as _, 0);
        })?.await?;

        Ok(res as _)
    }

    pub(crate) async fn processor_recv<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<usize> {
        Self::recv_with_flags(sock, buf, 0).await
    }

    pub(crate) async fn processor_peek<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<usize> {
        Self::recv_with_flags(sock, buf, libc::MSG_PEEK as _).await
    }

    async fn recv_with_flags<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
        flags: u32,
    ) -> io::Result<usize> {
        let fd = socket.as_raw_fd() as _;

        let res = Proactor::get().inner().register_io(|sqe| unsafe {
            let sqep = sqe.raw_mut();
            uring_sys::io_uring_prep_recv(sqep, fd, buf.as_ptr() as _, buf.len() as _, flags as _);
        })?.await?;

        Ok(res as _)
    }

    ///////////////////////////////////
    ///// Connect
    ///// Commonality of TcpStream, UdpSocket
    ///////////////////////////////////

    pub(crate) async fn processor_connect<A: ToSocketAddrs, F, Fut, T>(addrs: A, mut f: F) -> io::Result<T>
        where
            F: FnMut(SocketAddr) -> Fut,
            Fut: Future<Output = io::Result<T>>,
    {
        // TODO connect_tcp, connect_udp
        let addrs = match addrs.to_socket_addrs() {
            Ok(addrs) => addrs,
            Err(e) => return Err(e),
        };

        let mut tail_err = None;
        for addr in addrs {
            match f(addr).await {
                Ok(l) => return Ok(l),
                Err(e) => tail_err = Some(e),
            }
        }

        Err(tail_err.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "couldn't resolve addresses",
            )
        }))
    }

    pub(crate) async fn processor_connect_tcp(addr: SocketAddr) -> io::Result<Handle<TcpStream>> {
        let addr = addr.to_string();
        // FIXME: address resolution is always blocking.
        let addr = addr.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "could not resolve the address")
        })?;

        let domain = if addr.is_ipv6() {
            socket2::Domain::ipv6()
        } else {
            socket2::Domain::ipv4()
        };
        let sock = socket2::Socket::new(domain, socket2::Type::stream(), Some(socket2::Protocol::tcp()))?;

        sock.set_nonblocking(true)?;

        // FIXME: (vcq): iou uses nix, i use socket2, conversions happens over libc.
        // Propose std conversion for nix.
        let nixsaddr =
            unsafe {
                &iou::SockAddr::from_libc_sockaddr(sock.local_addr().unwrap().as_ptr()).unwrap()
            };
        let stream = sock.into_tcp_stream();
        let fd = stream.as_raw_fd() as _;

        Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_connect(fd, nixsaddr);
        })?.await?;

        Ok(Handle::new(stream)?)
    }

    pub(crate) async fn processor_connect_udp(addr: SocketAddr) -> io::Result<Handle<UdpSocket>> {
        let domain = match addr {
            SocketAddr::V4(_) => socket2::Domain::ipv4(),
            SocketAddr::V6(_) => socket2::Domain::ipv6(),
        };
        let sock = socket2::Socket::new(domain, socket2::Type::dgram(), Some(socket2::Protocol::udp()))?;
        let sockaddr = socket2::SockAddr::from(addr);

        let unspec = match addr {
            SocketAddr::V4(_) => {
                let unspecv4 = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
                socket2::SockAddr::from(unspecv4)
            }
            SocketAddr::V6(_) => {
                let unspecv6 = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
                socket2::SockAddr::from(unspecv6)
            }
        };

        // Try to bind to the datagram socket.
        sock.bind(&unspec)?;
        sock.set_nonblocking(true)?;

        // Try to connect over the socket
        sock.connect(&sockaddr)?;

        // Make into udp type and init handler.
        Ok(Handle::new(sock.into_udp_socket())?)
    }

    ///////////////////////////////////
    ///// TcpListener
    ///////////////////////////////////

    pub(crate) async fn processor_accept_tcp_listener<R: AsRawFd>(listener: &R) -> io::Result<(Handle<TcpStream>, SocketAddr)> {
        let fd = listener.as_raw_fd() as _;
        let mut saddrstor = SockAddrStorage::uninit();

        let cc = Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_accept(fd, Some(&mut saddrstor), SockFlag::empty())
        })?;
        dbg!("TCP LISTENER");

        let stream = unsafe { TcpStream::from_raw_fd(cc.await?) };
        dbg!("TCP LISTENER RECEIVED");
        let addr = unsafe {
            let nixsa = saddrstor.as_socket_addr()?;
            let (saddr, saddr_len) = nixsa.as_ffi_pair();
            socket2::SockAddr::from_raw_parts(saddr as *const _, saddr_len as _)
                .as_std()
                .unwrap()
        };

        Ok((Handle::new(stream)?, addr))
    }

    ///////////////////////////////////
    ///// UdpSocket
    ///////////////////////////////////

    pub(crate) async fn processor_send_to<R: AsRawFd>(
        socket: &R,
        buf: &[u8],
        addr: SocketAddr,
    ) -> io::Result<usize> {
        Self::send_to_dest(socket, buf, &socket2::SockAddr::from(addr)).await
    }

    async fn send_to_dest<A: AsRawFd>(socket: &A, buf: &[u8], addr: &socket2::SockAddr) -> io::Result<usize> {
        // FIXME: (vcq): Wrap into vec?
        let mut iov = IoSlice::new(buf);

        let mut sendmsg = unsafe { MaybeUninit::<libc::msghdr>::zeroed().assume_init() };
        sendmsg.msg_name = addr.as_ptr() as *mut _;
        sendmsg.msg_namelen = addr.len();
        sendmsg.msg_iov = iov.as_ptr() as *mut _;
        sendmsg.msg_iovlen = iov.len();

        let fd = socket.as_raw_fd() as _;

        let res = Proactor::get().inner().register_io(|sqe| unsafe {
            let sqep = sqe.raw_mut();
            uring_sys::io_uring_prep_sendmsg(sqep, fd, &sendmsg as *const _ as *const _, 0);
        })?.await?;

        Ok(res as _)
    }

    pub(crate) async fn processor_recv_from<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        Self::recv_from_with_flags(sock, buf, 0)
            .await
            .map(|(size, sockaddr)| (size, sockaddr.as_std().unwrap()))
    }

    pub(crate) async fn processor_peek_from<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        Self::recv_from_with_flags(sock, buf, libc::MSG_PEEK as _)
            .await
            .map(|(size, sockaddr)| (size, sockaddr.as_std().unwrap()))
    }

    async fn recv_from_with_flags<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
        flags: u32,
    ) -> io::Result<(usize, socket2::SockAddr)> {
        let mut sockaddr_raw = unsafe { MaybeUninit::<libc::sockaddr_storage>::zeroed().assume_init() };

        // FIXME: (vcq): Wrap into vec?
        let mut iov = IoSliceMut::new(buf);

        let mut recvmsg = unsafe { MaybeUninit::<libc::msghdr>::zeroed().assume_init() };
        recvmsg.msg_name = &mut sockaddr_raw as *mut _ as _;
        recvmsg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as _;
        recvmsg.msg_iov = iov.as_ptr() as *mut _;
        recvmsg.msg_iovlen = iov.len();

        let fd = socket.as_raw_fd() as _;

        let res = Proactor::get().inner().register_io(|sqe| unsafe {
            let sqep = sqe.raw_mut();
            uring_sys::io_uring_prep_recvmsg(
                sqep,
                fd,
                &mut recvmsg as *mut _ as *mut _,
                flags as _,
            );
        })?.await?;

        let sockaddr = unsafe {
            socket2::SockAddr::from_raw_parts(
                &sockaddr_raw as *const _ as *const _,
                recvmsg.msg_namelen,
            )
        };

        Ok((res as _, sockaddr))
    }

    ///////////////////////////////////
    ///// UnixListener
    ///////////////////////////////////

    pub(crate) async fn processor_accept_unix_listener<R: AsRawFd>(listener: &R) -> io::Result<(Handle<UnixStream>, UnixSocketAddr)> {
        let fd = listener.as_raw_fd() as _;
        let mut saddrstor = SockAddrStorage::uninit();

        let cc = Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_accept(fd, Some(&mut saddrstor), SockFlag::empty())
        })?;

        let stream = unsafe { UnixStream::from_raw_fd(cc.await?) };
        let addr = unsafe {
            let nixsa = saddrstor.as_socket_addr()?;
            let (saddr, saddr_len) = nixsa.as_ffi_pair();
            socket2::SockAddr::from_raw_parts(saddr as *const _, saddr_len as _)
        };
        let addr = shim_to_af_unix(&addr)?;

        Ok((Handle::new(stream)?, addr))
    }

    ///////////////////////////////////
    ///// UnixStream
    ///////////////////////////////////

    pub(crate) async fn processor_connect_unix<P: AsRef<Path>>(path: P) -> io::Result<Handle<UnixStream>> {
        let sock = socket2::Socket::new(socket2::Domain::unix(), socket2::Type::stream(), None)?;
        let sockaddr = socket2::SockAddr::unix(path)?;

        sock.set_nonblocking(true)?;

        // FIXME: (vcq): iou uses nix, i use socket2, conversions happens over libc.
        // Propose std conversion for nix.
        let nixsaddr =
            unsafe {
                &iou::SockAddr::from_libc_sockaddr(sock.local_addr().unwrap().as_ptr()).unwrap()
            };

        let stream = sock.into_unix_stream();
        let fd = stream.as_raw_fd() as _;

        Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_connect(fd, nixsaddr)
        })?.await?;

        Ok(Handle::new(stream)?)
    }

    pub(crate) async fn processor_send_to_unix<R: AsRawFd, P: AsRef<Path>>(socket: &R, buf: &[u8], path: P) -> io::Result<usize> {
        Self::send_to_dest(socket, buf, &socket2::SockAddr::unix(path)?).await
    }

    pub(crate) async fn processor_recv_from_unix<R: AsRawFd>(socket: &R, buf: &mut [u8]) -> io::Result<(usize, UnixSocketAddr)> {
        Self::recv_from_with_flags(socket, buf, 0)
            .await
            .map(|(size, sockaddr)| (size, shim_to_af_unix(&sockaddr).unwrap()))
    }

    pub(crate) async fn processor_peek_from_unix<R: AsRawFd>(socket: &R, buf: &mut [u8]) -> io::Result<(usize, UnixSocketAddr)> {
        Self::recv_from_with_flags(socket, buf, libc::MSG_PEEK as _)
            .await
            .map(|(size, sockaddr)| (size, shim_to_af_unix(&sockaddr).unwrap()))
    }
}