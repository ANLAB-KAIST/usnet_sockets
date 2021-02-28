/// singlethread version of the usnet_sockets API, without a background thread
/// The socket types here cannot be used by a multithreading application because
/// they are not movable and have no locking.
/// Assumption is that the application does no long-running computations and mainly
/// calls the socket operations because otherwise packets will be lost and timeouts missed.
/// The main purpose is evaluation of the overhead of background thread and locking
/// in the multithread version.
use smoltcp::time::Instant;
use std::io::{self, Write};
use std::os::unix::io::{AsRawFd, RawFd};

use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;

extern crate libusnetd;
use self::libusnetd::ClientMessageIp;
use smoltcp;
use smoltcp::socket::SocketSet;
use smoltcp::socket::{SocketHandle, TcpSocket, TcpSocketBuffer};
use smoltcp::wire::{IpAddress, IpProtocol};

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use std::net::{TcpListener as SystemTcpListener, TcpStream as SystemTcpStream};

use nix::poll::{poll, EventFlags, PollFd};
use std::os::raw::c_int;

use device::*;
use std::env;
use std::io::prelude::*;
use usnetconfig::*;

use rand::{thread_rng, Rng};

use serde_json;

pub struct StcpNetRef {
    pub r: Rc<RefCell<StcpNet>>,
}
pub struct StcpListenerRef {
    pub l: Rc<RefCell<StcpListener>>,
}

thread_local! {
  pub static STCP_LOCAL: RefCell<Option<StcpNetRef>> = RefCell::new(None);
}

fn init_thread() {
    STCP_LOCAL.with(|stcp| {
        let _ = stcp.borrow_mut().get_or_insert(StcpNetRef {
            r: StcpNet::new_from_env(),
        });
    });
}

pub struct TcpListener {}

impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<StcpListenerRef> {
        init_thread();
        STCP_LOCAL.with(|stcp| stcp.borrow().as_ref().unwrap().bind(addr))
    }
}

pub struct StcpNet {
    sockets: SocketSet<'static>,
    iface: StcpBackendInterface,
    fd: RawFd,
    waiting_poll: bool,
    socket_buffer_size: usize,
}

impl StcpNet {
    pub fn new_from_env() -> Rc<RefCell<StcpNet>> {
        let confvar = env::var("USNET_SOCKETS").unwrap();
        let conf: StcpBackend = serde_json::from_str(&confvar).unwrap();
        let waiting_poll = env::var("USNET_SOCKETS_WAIT").unwrap_or("true".to_string()) == "true";
        // Not touched in the evaluation, should normally stay "true" and was only used with "false" for testing busy polling with netmap
        info!("USNET_SOCKETS_WAIT: {}", waiting_poll);
        let socket_buffer_size =
            usize::from_str(&env::var("SOCKET_BUFFER").unwrap_or("500000".to_string()))
                .expect("SOCKET_BUFFER not an usize");
        assert!(socket_buffer_size > 0);
        info!("SOCKET_BUFFER: {}", socket_buffer_size);
        let reduce_mtu_by_nr =
            usize::from_str(&env::var("REDUCE_MTU_BY").unwrap_or("0".to_string()))
                .expect("BG_THREAD_PIN_CPU_ID not a number");
        let reduce_mtu_by = if reduce_mtu_by_nr == 0 {
            None
        } else {
            Some(reduce_mtu_by_nr)
        };
        info!("REDUCE_MTU_BY: {:?}", reduce_mtu_by);
        StcpNet::new(conf, waiting_poll, socket_buffer_size, reduce_mtu_by)
    }
    pub fn new(
        backend: StcpBackend,
        waiting_poll: bool,
        socket_buffer_size: usize,
        reduce_mtu_by: Option<usize>,
    ) -> Rc<RefCell<StcpNet>> {
        let (fd, iface_backend) = backend.to_interface(waiting_poll, reduce_mtu_by);
        info!("created backend: {}", iface_backend);
        Rc::new(RefCell::new(StcpNet {
            sockets: SocketSet::new(vec![]),
            fd: fd,
            iface: iface_backend,
            waiting_poll: waiting_poll,
            socket_buffer_size: socket_buffer_size,
        }))
    }
    fn create_socket(&mut self) -> SocketHandle {
        let tcp_rx_buffer = TcpSocketBuffer::new(vec![0; self.socket_buffer_size]);
        let tcp_tx_buffer = TcpSocketBuffer::new(vec![0; self.socket_buffer_size]);
        let tcp_socket = TcpSocket::new(tcp_rx_buffer, tcp_tx_buffer);
        self.sockets.add(tcp_socket)
    }
    pub fn poll_wait(&mut self, once: bool, poll_other: Option<&[RawFd]>) {
        let mut fds = vec![PollFd::new(self.fd, EventFlags::POLLIN)];
        if let Some(poll_others) = poll_other {
            fds.extend(
                poll_others
                    .iter()
                    .map(|fd| PollFd::new(*fd, EventFlags::POLLIN)),
            );
        }
        while !match self.iface.poll(&mut self.sockets, Instant::now()) {
            Err(err) => {
                debug!("poll result: {}", err);
                true
            }
            Ok(r) => {
                let others = poll_other.is_some()
                    && fds[1..]
                        .iter()
                        .filter(|o| o.revents() == Some(EventFlags::POLLIN))
                        .next()
                        .is_some();
                r || others
            } // readiness changed
        } {
            if once {
                break;
            }
            if !self.waiting_poll {
                break;
            }
            let d = self.iface.poll_delay(&self.sockets, Instant::now());
            let d_cint = match d {
                Some(duration) => duration.total_millis() as c_int,
                None => -1 as c_int,
            };
            poll(&mut fds[..], d_cint).expect("wait error");
        }
    }
}

impl StcpNetRef {
    fn bind<A: ToSocketAddrs>(&self, addr: A) -> io::Result<StcpListenerRef> {
        let sockaddrs: Vec<SocketAddr> = addr.to_socket_addrs().unwrap().collect();
        let mut sockaddr = sockaddrs[0];
        let ipa = sockaddr.ip();

        let tcp_handle;
        let lolisten;
        {
            let mut stcpnet = self.r.borrow_mut();

            let ipv4 = stcpnet.iface.ips()[0].address();
            match stcpnet.iface.control() {
                Some(control) => {
                    if sockaddr.port() != 0 {
                        if !is_free_listening_port_for(
                            control,
                            sockaddr.port(),
                            ClientMessageIp::Ipv4(format!("{}", ipv4)),
                        ) {
                            return Err(io::Error::new(io::ErrorKind::Other, "port not free"));
                        }
                    } else {
                        let local_port = find_free_connecting_or_listening_port_for(
                            false,
                            control,
                            ClientMessageIp::Ipv4(format!("{}", ipv4)),
                        );
                        sockaddr.set_port(local_port);
                    }
                }
                _ => {
                    if sockaddr.port() == 0 {
                        let local_port: u16 = 1u16 + thread_rng().gen_range(1024, std::u16::MAX);
                        sockaddr.set_port(local_port);
                    }
                }
            };

            let mut loaddr = sockaddr.clone();
            if loaddr.ip().is_unspecified() {
                loaddr.set_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
            }
            let lolisten_res = SystemTcpListener::bind(loaddr);
            if sockaddr.ip().is_loopback() {
                return Ok(StcpListenerRef {
                    l: Rc::new(RefCell::new(StcpListener {
                        port: sockaddr.port(),
                        stcpnet: self.r.clone(),
                        lo: lolisten_res?,
                        listen_handles: None,
                    })),
                });
            }
            lolisten = lolisten_res?;
            lolisten.set_nonblocking(true)?;

            tcp_handle = stcpnet.create_socket();
            {
                let mut socket = stcpnet.sockets.get::<TcpSocket>(tcp_handle);
                socket.listen(sockaddr.port()).unwrap();
            }
            stcpnet.iface.add_port_match(
                ipa,
                Some(sockaddr.port()),
                None,
                None,
                IpProtocol::Tcp,
            )?;
        }
        Ok(StcpListenerRef {
            l: Rc::new(RefCell::new(StcpListener {
                port: sockaddr.port(),
                stcpnet: self.r.clone(),
                listen_handles: Some(vec![tcp_handle]),
                lo: lolisten,
            })),
        })
    }
    fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<TcpStream> {
        let addr = addr.to_socket_addrs().unwrap().next().unwrap();

        let ipv4 = {
            let stcpnet = self.r.borrow();
            stcpnet.iface.ips()[0].address()
        };
        if addr.ip().is_loopback() || IpAddress::from(addr.ip()) == ipv4 {
            return SystemTcpStream::connect(addr).map(|s| TcpStream::System(s));
        }

        let tcp_handle;
        {
            let mut stcpnet = self.r.borrow_mut();

            let mut local_port: u16 = 1u16 + thread_rng().gen_range(1024, std::u16::MAX);

            let ipv4 = stcpnet.iface.ips()[0].address();
            match stcpnet.iface.control() {
                Some(control) => {
                    local_port = find_free_connecting_or_listening_port_for(
                        true,
                        control,
                        ClientMessageIp::Ipv4(format!("{}", ipv4)),
                    );
                }
                _ => {}
            }

            tcp_handle = stcpnet.create_socket();
            let mut socket = stcpnet.sockets.get::<TcpSocket>(tcp_handle);

            socket.connect(addr, local_port).unwrap();
        }
        loop {
            {
                let mut stcpnet = self.r.borrow_mut();
                stcpnet.poll_wait(false, None);
                let socket = stcpnet.sockets.get::<TcpSocket>(tcp_handle);
                if socket.is_active() && socket.may_recv() && socket.may_send() {
                    break;
                }
            }
        }
        Ok(StcpStream {
            stcpnet: self.r.clone(),
            sockethandle: tcp_handle,
        })
        .map(|s| TcpStream::Stcp(s))
    }
}

pub struct StcpListener {
    stcpnet: Rc<RefCell<StcpNet>>,
    listen_handles: Option<Vec<SocketHandle>>,
    lo: SystemTcpListener,
    port: u16,
}

pub struct StcpStream {
    sockethandle: SocketHandle,
    stcpnet: Rc<RefCell<StcpNet>>,
}

pub struct StcpListenerIt {
    listener: StcpListenerRef,
}

impl Iterator for StcpListenerIt {
    type Item = io::Result<TcpStream>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.listener.accept() {
            Ok((stream, _)) => Some(Ok(stream)),
            _ => None,
        }
    }
}

impl StcpListenerRef {
    pub fn incoming(&self) -> StcpListenerIt {
        StcpListenerIt {
            listener: StcpListenerRef { l: self.l.clone() },
        }
    }
    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let mut listener = self.l.borrow_mut();
        if listener.listen_handles.is_none() {
            return listener.lo.accept().map(|(s, a)| (TcpStream::System(s), a));
        }
        let mut first = true;
        loop {
            let mut r = None;
            {
                let mut stcpnet = listener.stcpnet.borrow_mut();
                stcpnet.poll_wait(first, Some(&[listener.lo.as_raw_fd()]));
                first = false;
                let sys_res = listener.lo.accept();
                if let Ok((s, a)) = sys_res {
                    s.set_nonblocking(false)
                        .expect("couldn't set nonblocking option");
                    return Ok((TcpStream::System(s), a));
                }

                for (i, handle) in listener.listen_handles.as_ref().unwrap().iter().enumerate() {
                    let socket = stcpnet.sockets.get::<TcpSocket>(*handle);
                    if socket.is_active() && socket.may_recv() && socket.may_send() {
                        let ep = socket.remote_endpoint();
                        let stip = match ep.addr {
                            IpAddress::Ipv4(v) => IpAddr::V4(Ipv4Addr::from(v)),
                            IpAddress::Ipv6(v) => IpAddr::V6(Ipv6Addr::from(v)),
                            _ => panic!("not covered"),
                        };
                        let sadd: SocketAddr = SocketAddr::new(stip, ep.port);
                        r = Some((
                            i,
                            Ok((
                                TcpStream::Stcp(StcpStream {
                                    sockethandle: *handle,
                                    stcpnet: listener.stcpnet.clone(),
                                }),
                                sadd,
                            )),
                        ));
                        break;
                    }
                }
            }
            match r {
                Some((i, r)) => {
                    let tcp_handle;
                    listener.listen_handles.as_mut().unwrap().remove(i);
                    {
                        let mut stcpnet = listener.stcpnet.borrow_mut();
                        tcp_handle = stcpnet.create_socket();
                        let mut socket = stcpnet.sockets.get::<TcpSocket>(tcp_handle);
                        socket.listen(listener.port).unwrap();
                    }
                    listener.listen_handles.as_mut().unwrap().push(tcp_handle);
                    return r;
                }
                _ => {}
            }
        }
    }
}

pub enum TcpStream {
    System(SystemTcpStream),
    Stcp(StcpStream),
}

impl Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TcpStream::Stcp(slf) => slf.read(buf),
            TcpStream::System(slf) => slf.read(buf),
        }
    }
}

impl Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TcpStream::Stcp(slf) => slf.write(buf),
            TcpStream::System(slf) => slf.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            TcpStream::Stcp(slf) => slf.flush(),
            TcpStream::System(slf) => slf.flush(),
        }
    }
}

impl TcpStream {
    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        init_thread();
        STCP_LOCAL.with(|stcp| stcp.borrow().as_ref().unwrap().connect(addr))
    }

    pub fn write_no_copy<F, R>(&mut self, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> (usize, R),
    {
        match self {
            TcpStream::Stcp(slf) => {
                let mut first = true;
                while {
                    let mut stcpnet = slf.stcpnet.borrow_mut();
                    let socket = stcpnet.sockets.get::<TcpSocket>(slf.sockethandle);
                    socket.may_send() && !socket.can_send()
                } {
                    slf.stcpnet.borrow_mut().poll_wait(first, None);
                    first = false;
                }
                let mut stcpnet = slf.stcpnet.borrow_mut();
                let mut socket = stcpnet.sockets.get::<TcpSocket>(slf.sockethandle);
                return socket.send(f);
            }
            TcpStream::System(slf) => {
                let mut g = vec![0; 8];
                let (u, r) = f(&mut g[..]);
                match slf.write_all(&g[..u]) {
                    Ok(_) => Ok(r),
                    _ => Err(smoltcp::Error::Exhausted),
                }
            }
        }
    }
    pub fn read_no_copy<F, R>(&mut self, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> (usize, R),
    {
        match self {
            TcpStream::Stcp(slf) => {
                let mut first = true;
                while {
                    let mut stcpnet = slf.stcpnet.borrow_mut();
                    let socket = stcpnet.sockets.get::<TcpSocket>(slf.sockethandle);
                    socket.may_recv() && !socket.can_recv()
                } {
                    slf.stcpnet.borrow_mut().poll_wait(first, None);
                    first = false;
                }
                let mut stcpnet = slf.stcpnet.borrow_mut();
                let mut socket = stcpnet.sockets.get::<TcpSocket>(slf.sockethandle);
                socket.recv(f)
            }
            TcpStream::System(slf) => {
                let mut g = vec![0; 8];
                match slf.peek(&mut g[..]) {
                    Ok(v) => {
                        g.resize(v, 0);
                        let (u, r) = f(&mut g[..]);
                        match slf.read(&mut g[..u]) {
                            Ok(k) if k == u => Ok(r),
                            _ => Err(smoltcp::Error::Exhausted),
                        }
                    }
                    _ => Err(smoltcp::Error::Exhausted),
                }
            }
        }
    }
}

impl Drop for StcpStream {
    fn drop(&mut self) {
        let mut stcpnet = self.stcpnet.borrow_mut();
        {
            let mut socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
            socket.close();
        }
        stcpnet.sockets.release(self.sockethandle);
        debug!("drop-closing");
    }
}

impl StcpStream {
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let bufsize = buf.len();
        let mut data_read = 0;
        while {
            let mut stcpnet = self.stcpnet.borrow_mut();
            let socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
            socket.may_recv()
        } {
            let mut stcpnet = self.stcpnet.borrow_mut();
            {
                let mut socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
                let rres = socket.recv(|buffer| {
                    if buffer.len() >= bufsize - data_read {
                        let out = bufsize - data_read;
                        buf[data_read..].copy_from_slice(&buffer[..out]);
                        data_read = bufsize;
                        (out, ())
                    } else {
                        if buffer.len() > 0 {
                            buf[data_read..(data_read + buffer.len())].copy_from_slice(&buffer[..]);
                            data_read += buffer.len();
                        }
                        (buffer.len(), ())
                    }
                });
                match rres {
                    Ok(_) => {
                        if data_read == 0 {
                            // return 0 read if closed first time, then close fully socket.close()?
                            if !socket.may_recv() {
                                return Ok(0);
                            }
                        } else {
                            return Ok(data_read);
                        }
                    }
                    _ => {
                        return Err(io::Error::new(io::ErrorKind::Other, "tbd r"));
                    } // socket is now closed
                }
            }
            stcpnet.poll_wait(false, None);
        }
        return Ok(data_read);
    }

    pub fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let bufsize = buf.len();
        let mut data_written = 0;
        while {
            let mut stcpnet = self.stcpnet.borrow_mut();
            let socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
            socket.may_send()
        } {
            let mut stcpnet = self.stcpnet.borrow_mut();
            {
                let mut socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
                let sres = socket.send(|buffer| {
                    if buffer.len() >= bufsize - data_written {
                        let out = bufsize - data_written;
                        buffer[..out].copy_from_slice(&buf[data_written..]);
                        data_written = bufsize;
                        (out, ())
                    } else {
                        let buffer_len = buffer.len();
                        if buffer_len > 0 {
                            buffer[..]
                                .copy_from_slice(&buf[data_written..(data_written + buffer_len)]);
                            data_written += buffer_len;
                        }
                        (buffer_len, ())
                    }
                });
                match sres {
                    Ok(_) => {
                        if data_written > 0 {
                            return Ok(data_written);
                        }
                    }
                    _ => {
                        return Err(io::Error::new(io::ErrorKind::Other, "tbd w1"));
                    }
                };
            }
            stcpnet.poll_wait(false, None);
        }
        return Err(io::Error::new(io::ErrorKind::Other, "tbd w2"));
    }
    pub fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
