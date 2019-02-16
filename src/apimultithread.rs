/// multithread (and default) version of the usnet_sockets API, uses a background thread
/// This API aims for compatibility with the socket types TcpStream and TcpListener
/// of the Rust standard library.
/// In contrast to the singlethread version, the socket types here can be used by a
/// multithreading application because they are movable and have locking.
/// Since there is a background thread which handles the socket timeouts and incoming
/// packets, the application may do longer computations without calling socket operations.
use parking_lot::{Condvar, Mutex};
use smoltcp::time::Instant;
use std::io::{self, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;

use smoltcp;
use smoltcp::socket::SocketSet;
use smoltcp::socket::{SocketHandle, TcpSocket, TcpSocketBuffer};
use smoltcp::wire::{IpAddress, IpEndpoint, IpProtocol};

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::net::{Shutdown, TcpListener as SystemTcpListener, TcpStream as SystemTcpStream};
use std::os::unix::net::UnixDatagram;

use nix::poll::{poll, EventFlags, PollFd};
use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::Pid;
use std::os::raw::c_int;

use config::*;
use device::*;
use rand::{thread_rng, Rng};
use std::env;
use std::io::prelude::*;
use system::read_kernel_local_port_range;

use serde_json;

#[derive(Clone)]
pub struct StcpNetRef {
    pub r: Arc<(Mutex<StcpNet>, Condvar)>,
}

pub struct StcpListenerRef {
    pub l: Arc<Mutex<StcpListener>>,
}

lazy_static! {
    static ref STCP_GLOBAL: StcpNetRef = StcpNetRef {
        r: Arc::new((Mutex::new(StcpNet::new_from_env()), Condvar::new()))
    };
}

pub fn bg() {
    let mut dummy = vec![0; 1000];
    let mut fds = vec![];
    let mut fds_extra = vec![];
    let mut waiting_poll;
    let mut skip;

    {
        let &(ref stcpnetref, ref _cond) = &*STCP_GLOBAL.r;
        let stcpnet = stcpnetref.lock();
        match stcpnet.bg_thread_pin_cpu_id {
            Some(cpuid) => {
                let mut cpus = CpuSet::new();
                cpus.set(cpuid).unwrap();
                sched_setaffinity(Pid::from_raw(0), &cpus).unwrap();
            }
            _ => {}
        }
    }

    loop {
        let delay = {
            let &(ref stcpnetref, ref _cond) = &*STCP_GLOBAL.r;
            let mut stcpnet = stcpnetref.lock();
            waiting_poll = stcpnet.waiting_poll;
            if stcpnet.bg_skip_one_wait == Skip::Skip {
                skip = true;
            } else {
                skip = false;
                stcpnet.bg_skip_one_wait = Skip::Wait;
            }
            if !stcpnet.fds_add.is_empty() {
                fds_extra.extend_from_slice(&stcpnet.fds_add);
                stcpnet.fds_add.clear();
                fds.clear();
            }
            if !stcpnet.fds_remove.is_empty() {
                for fdi in stcpnet.fds_remove.iter() {
                    let pos = fds_extra.iter().position(|x| *x == *fdi).unwrap();
                    let _ = fds_extra.remove(pos);
                }
                stcpnet.fds_remove.clear();
                fds.clear();
            }
            if fds.is_empty() {
                fds.push(PollFd::new(stcpnet.fd, EventFlags::POLLIN));
                fds.push(PollFd::new(
                    stcpnet.notify_poll_listener.as_raw_fd(),
                    EventFlags::POLLIN,
                ));
                fds.extend(
                    fds_extra
                        .iter()
                        .map(|fd| PollFd::new(*fd, EventFlags::POLLIN)),
                );
            }

            let d = stcpnet.iface.poll_delay(&stcpnet.sockets, Instant::now());
            let delay = match &d {
                Some(duration) => duration.total_millis() as c_int,
                None => -1 as c_int,
            };
            stcpnet.current_wait_delay = delay;
            delay
        };
        if waiting_poll && !skip {
            poll(&mut fds[..], delay).expect("wait error");
        }
        {
            let &(ref stcpnetref, ref cond) = &*STCP_GLOBAL.r;
            let mut stcpnet = stcpnetref.lock();
            while let Ok(_) = stcpnet.notify_poll_listener.recv(&mut dummy) {
                // consume all notifications
            }
            let _readyness_changed = stcpnet.poll();
            stcpnet.bg_skip_one_wait = Skip::Skippable;
            cond.notify_all();
        }
    }
}

pub struct TcpListener {}

impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<StcpListenerRef> {
        STCP_GLOBAL.bind(addr)
    }
}

#[derive(PartialEq, Eq)]
enum Skip {
    Wait,
    Skippable,
    Skip,
}

pub struct StcpNet {
    sockets: SocketSet<'static, 'static, 'static>,
    iface: StcpBackendInterface,
    bg: JoinHandle<()>,
    fd: RawFd,
    waiting_poll: bool,
    notify_poll: UnixDatagram,
    notify_poll_listener: UnixDatagram,
    fds_add: Vec<RawFd>,
    fds_remove: Vec<RawFd>,
    bg_skip_one_wait: Skip, // temporary skip one poll call
    socket_backlog: usize,  // max number of parallel incoming SYNs
    current_wait_delay: c_int,
    socket_buffer_size: usize,
    bg_thread_pin_cpu_id: Option<usize>,
    kernel_local_port_range: (u16, u16),
}

impl StcpNet {
    pub fn poll(&mut self) -> bool {
        match self.iface.poll(&mut self.sockets, Instant::now()) {
            Ok(r) => r,
            Err(err) => {
                debug!("poll result: {}", err);
                true
            }
        }
    }
    fn create_socket(&mut self) -> SocketHandle {
        let tcp_rx_buffer = TcpSocketBuffer::new(vec![0; self.socket_buffer_size]);
        let tcp_tx_buffer = TcpSocketBuffer::new(vec![0; self.socket_buffer_size]);
        let tcp_socket = TcpSocket::new(tcp_rx_buffer, tcp_tx_buffer);
        self.sockets.add(tcp_socket)
    }
    pub fn new_from_env() -> StcpNet {
        let confvar = env::var("USNET_SOCKETS").unwrap();
        let conf: StcpBackend = serde_json::from_str(&confvar).unwrap();
        let waiting_poll = env::var("USNET_SOCKETS_WAIT").unwrap_or("true".to_string()) == "true";
        info!("USNET_SOCKETS_WAIT: {}", waiting_poll);
        let socket_buffer_size =
            usize::from_str(&env::var("SOCKET_BUFFER").unwrap_or("500000".to_string()))
                .expect("SOCKET_BUFFER not an usize");
        assert!(socket_buffer_size > 0);
        info!("SOCKET_BUFFER: {}", socket_buffer_size);
        let bg_thread_pin_cpu_id_nr =
            i16::from_str(&env::var("BG_THREAD_PIN_CPU_ID").unwrap_or("-1".to_string()))
                .expect("BG_THREAD_PIN_CPU_ID not a number");
        let bg_thread_pin_cpu_id = if bg_thread_pin_cpu_id_nr < 0 {
            None
        } else {
            Some(bg_thread_pin_cpu_id_nr as usize)
        };
        info!("BG_THREAD_PIN_CPU_ID: {:?}", bg_thread_pin_cpu_id);
        let socket_backlog =
            usize::from_str(&env::var("SOCKET_BACKLOG").unwrap_or("32".to_string()))
                .expect("SOCKET_BACKLOG not an usize");
        assert!(socket_backlog > 0);
        info!("SOCKET_BACKLOG: {}", socket_backlog);
        let reduce_mtu_by_nr =
            usize::from_str(&env::var("REDUCE_MTU_BY").unwrap_or("0".to_string()))
                .expect("BG_THREAD_PIN_CPU_ID not a number");
        let reduce_mtu_by = if reduce_mtu_by_nr == 0 {
            None
        } else {
            Some(reduce_mtu_by_nr)
        };
        info!("REDUCE_MTU_BY: {:?}", reduce_mtu_by);
        StcpNet::new(
            conf,
            waiting_poll,
            socket_buffer_size,
            bg_thread_pin_cpu_id,
            socket_backlog,
            reduce_mtu_by,
        )
    }
    pub fn new(
        backend: StcpBackend,
        waiting_poll: bool,
        socket_buffer_size: usize,
        bg_thread_pin_cpu_id: Option<usize>,
        socket_backlog: usize,
        reduce_mtu_by: Option<usize>,
    ) -> StcpNet {
        let (fd, iface_backend) = backend.to_interface(waiting_poll, reduce_mtu_by);
        info!("created backend: {}", iface_backend);
        let (notify_poll, notify_poll_listener) = UnixDatagram::pair().unwrap();
        let _ = notify_poll.set_nonblocking(true).unwrap();
        let _ = notify_poll_listener.set_nonblocking(true).unwrap();
        StcpNet {
            current_wait_delay: -1 as c_int,
            bg_skip_one_wait: Skip::Wait,
            bg: thread::spawn(bg),
            fd: fd,
            fds_add: vec![],
            fds_remove: vec![],
            sockets: SocketSet::new(vec![]),
            notify_poll: notify_poll,
            notify_poll_listener: notify_poll_listener,
            iface: iface_backend,
            waiting_poll: waiting_poll,
            socket_backlog: socket_backlog,
            socket_buffer_size: socket_buffer_size,
            bg_thread_pin_cpu_id: bg_thread_pin_cpu_id,
            kernel_local_port_range: read_kernel_local_port_range(),
        }
    }
}

impl StcpNetRef {
    fn bind<A: ToSocketAddrs>(&self, addr: A) -> io::Result<StcpListenerRef> {
        let mut r = Err(io::Error::new(
            io::ErrorKind::Other,
            "to_socket_addrs is empty",
        ));
        for mut sockaddr in addr.to_socket_addrs()? {
            let ipa = sockaddr.ip();
            if !ipa.is_loopback() && ipa.is_ipv6() {
                r = Err(io::Error::new(io::ErrorKind::Other, "ipv6 bind"));
                continue;
            }

            let mut listen_handles = vec![];
            let lolisten;
            {
                let &(ref stcpnetref, ref _cond) = &*self.r;
                let mut stcpnet = stcpnetref.lock();

                let mut lo_result = Err(io::Error::new(io::ErrorKind::Other, "uninit"));
                let mut usnet_result = Err(io::Error::new(io::ErrorKind::Other, "uninit"));
                let find_random = sockaddr.port() == 0;
                while lo_result.is_err() && (usnet_result.is_err() || sockaddr.ip().is_loopback()) {
                    if find_random {
                        let local_port: u16 = 1u16 + thread_rng().gen_range(1024, std::u16::MAX);
                        sockaddr.set_port(local_port);
                    }
                    let mut loaddr = sockaddr.clone();
                    if loaddr.ip().is_unspecified() {
                        loaddr.set_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
                    }
                    lo_result = SystemTcpListener::bind(loaddr);
                    if lo_result.is_ok() && !sockaddr.ip().is_loopback() {
                        usnet_result = stcpnet.iface.add_port_match(
                            ipa,
                            Some(sockaddr.port()),
                            None,
                            None,
                            IpProtocol::Tcp,
                        );
                    }
                    if !find_random {
                        break;
                    }
                }

                match lo_result {
                    Ok(l) => lolisten = l,
                    Err(e) => {
                        r = Err(e);
                        continue;
                    }
                }
                if sockaddr.ip().is_loopback() {
                    return Ok(StcpListenerRef {
                        l: Arc::new(Mutex::new(StcpListener {
                            stcpnet: (*self).clone(),
                            port: sockaddr.port(),
                            lo: lolisten,
                            listen_handles: None,
                        })),
                    });
                }
                match usnet_result {
                    Ok(_) => {}
                    Err(e) => {
                        r = Err(e);
                        continue;
                    }
                }
                lolisten.set_nonblocking(true)?;
                stcpnet.fds_add.push(lolisten.as_raw_fd());

                for _ in 0..stcpnet.socket_backlog {
                    let tcp_handle = stcpnet.create_socket();
                    let mut socket = stcpnet.sockets.get::<TcpSocket>(tcp_handle);
                    socket.listen(sockaddr.port()).unwrap();
                    listen_handles.push(tcp_handle);
                }
            }
            return Ok(StcpListenerRef {
                l: Arc::new(Mutex::new(StcpListener {
                    stcpnet: (*self).clone(),
                    port: sockaddr.port(),
                    listen_handles: Some(listen_handles),
                    lo: lolisten,
                })),
            });
        }
        r
    }
    fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<TcpStream> {
        let mut r = Err(io::Error::new(
            io::ErrorKind::Other,
            "to_socket_addrs is empty",
        ));
        for addr in addr.to_socket_addrs()? {
            let mut error = false;
            let tcp_handle;
            {
                let &(ref stcpnetref, ref cond) = &*self.r;
                let mut stcpnet = stcpnetref.lock();

                let ipv4 = stcpnet.iface.ips()[0].address();
                if addr.ip().is_loopback()
                    || IpAddress::from(addr.ip()) == ipv4
                    || addr.ip().is_unspecified()
                {
                    r = SystemTcpStream::connect(addr).map(|s| TcpStream::System(s));
                    if r.is_ok() {
                        return r;
                    } else {
                        continue;
                    }
                }
                if addr.is_ipv6() {
                    r = Err(io::Error::new(io::ErrorKind::Other, "ipv6 address"));
                    continue;
                }

                let mut local_port: u16;
                loop {
                    local_port = 1u16 + thread_rng().gen_range(1024, std::u16::MAX);
                    let (lower, upper) = stcpnet.kernel_local_port_range;
                    if local_port >= lower && local_port <= upper {
                        continue;
                    }
                    let own_ip = match ipv4.into() {
                        IpAddress::Ipv4(v) => IpAddr::V4(Ipv4Addr::from(v)),
                        IpAddress::Ipv6(v) => IpAddr::V6(Ipv6Addr::from(v)),
                        IpAddress::Unspecified => IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                        IpAddress::__Nonexhaustive => panic!("not covered"),
                    };
                    if stcpnet
                        .iface
                        .add_port_match(
                            own_ip,
                            Some(local_port),
                            Some(addr.ip()),
                            Some(addr.port()),
                            IpProtocol::Tcp,
                        )
                        .is_ok()
                    {
                        break;
                    }
                }

                tcp_handle = stcpnet.create_socket();
                {
                    let mut socket = stcpnet.sockets.get::<TcpSocket>(tcp_handle);
                    socket.connect(addr, local_port).unwrap();
                }

                if stcpnet.bg_skip_one_wait == Skip::Wait {
                    let _ = stcpnet.notify_poll.send(b"$").unwrap();
                } else {
                    stcpnet.bg_skip_one_wait = Skip::Skip;
                }

                while !{
                    let socket = stcpnet.sockets.get::<TcpSocket>(tcp_handle);
                    if !socket.is_active() {
                        error = true;
                        true // break
                    } else {
                        socket.may_recv() && socket.may_send()
                    }
                } {
                    cond.wait(&mut stcpnet);
                }
            }
            if error {
                r = Err(io::Error::new(
                    io::ErrorKind::Other,
                    "connection not successful",
                ));
            } else {
                return Ok(TcpStream::Stcp(StcpStream {
                    stcpnet: (*self).clone(),
                    sockethandle: tcp_handle,
                }));
            }
        }
        r
    }
}

pub struct StcpListener {
    stcpnet: StcpNetRef,
    listen_handles: Option<Vec<SocketHandle>>,
    lo: SystemTcpListener,
    port: u16,
}

impl Drop for StcpListener {
    fn drop(&mut self) {
        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        stcpnet.fds_remove.push(self.lo.as_raw_fd());
        if let Some(ref listen_handles) = self.listen_handles {
            for listen_handle in listen_handles.iter() {
                debug!("drop-closing listener");
                stcpnet.sockets.release(*listen_handle);
                stcpnet.sockets.prune();
            }
        }
    }
}

pub struct StcpStream {
    stcpnet: StcpNetRef,
    sockethandle: SocketHandle,
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
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let listener = self.l.lock();
        match listener.listen_handles {
            None => {
                return listener.lo.local_addr();
            }
            Some(ref listen_handles) => {
                let &(ref stcpnetref, ref _cond) = &*listener.stcpnet.r;
                let mut stcpnet = stcpnetref.lock();
                let handle = listen_handles[0];
                let socket = stcpnet.sockets.get::<TcpSocket>(handle);
                let local = socket.local_endpoint();
                Ok(endpoint_to_socket_addr(&local))
            }
        }
    }
    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let mut listener = self.l.lock();
        if listener.listen_handles.is_none() {
            return listener.lo.accept().map(|(s, a)| (TcpStream::System(s), a));
        }
        loop {
            let mut r = None;
            {
                let &(ref stcpnetref, ref cond) = &*listener.stcpnet.r;
                let mut stcpnet = stcpnetref.lock();

                cond.wait(&mut stcpnet);

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
                        let sadd = endpoint_to_socket_addr(&ep);
                        r = Some((
                            i,
                            Ok((
                                TcpStream::Stcp(StcpStream {
                                    stcpnet: listener.stcpnet.clone(),
                                    sockethandle: *handle,
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
                    listener.listen_handles.as_mut().unwrap().remove(i);
                    let tcp_handle;
                    {
                        let &(ref stcpnetref, ref _cond) = &*listener.stcpnet.r;
                        let mut stcpnet = stcpnetref.lock();
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
        STCP_GLOBAL.connect(addr)
    }
    pub fn try_clone(&self) -> io::Result<TcpStream> {
        match self {
            TcpStream::Stcp(slf) => Ok(TcpStream::Stcp(slf.try_clone()?)),
            TcpStream::System(slf) => Ok(TcpStream::System(slf.try_clone()?)),
        }
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            TcpStream::Stcp(slf) => slf.local_addr(),
            TcpStream::System(slf) => slf.local_addr(),
        }
    }
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            TcpStream::Stcp(slf) => slf.peer_addr(),
            TcpStream::System(slf) => slf.peer_addr(),
        }
    }
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            TcpStream::Stcp(slf) => slf.shutdown(how),
            TcpStream::System(slf) => slf.shutdown(how),
        }
    }

    pub fn write_no_copy<F, R>(&mut self, f: F) -> smoltcp::Result<R>
    // returns smoltcp err Illegal instead of io err ConnectionReset
    where
        F: FnOnce(&mut [u8]) -> (usize, R),
    {
        match self {
            TcpStream::Stcp(slf) => {
                let &(ref stcpnetref, ref cond) = &*slf.stcpnet.r;
                let mut stcpnet = stcpnetref.lock();

                while {
                    let socket = stcpnet.sockets.get::<TcpSocket>(slf.sockethandle);
                    if !socket.may_send() {
                        return Err(smoltcp::Error::Illegal);
                    }
                    !socket.can_send()
                } {
                    cond.wait(&mut stcpnet);
                }
                let r;
                {
                    let mut socket = stcpnet.sockets.get::<TcpSocket>(slf.sockethandle);
                    r = socket.send(f);
                }
                if stcpnet.bg_skip_one_wait == Skip::Wait {
                    let _ = stcpnet.notify_poll.send(b"$").unwrap();
                } else {
                    stcpnet.bg_skip_one_wait = Skip::Skip;
                }
                r
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
                let &(ref stcpnetref, ref cond) = &*slf.stcpnet.r;
                let mut stcpnet = stcpnetref.lock();
                while {
                    let socket = stcpnet.sockets.get::<TcpSocket>(slf.sockethandle);
                    if !socket.may_recv() {
                        let (_, rt) = f(&mut []);
                        return Ok(rt);
                    }
                    !socket.can_recv()
                } {
                    cond.wait(&mut stcpnet);
                }
                let r;
                {
                    let mut socket = stcpnet.sockets.get::<TcpSocket>(slf.sockethandle);
                    r = socket.recv(f);
                }
                if stcpnet.bg_skip_one_wait == Skip::Wait
                    && (stcpnet.current_wait_delay == -1 as c_int
                        || stcpnet.current_wait_delay > 10 as c_int)
                {
                    let _ = stcpnet.notify_poll.send(b"$").unwrap();
                }
                r
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
        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        debug!("drop-closing");
        stcpnet.sockets.release(self.sockethandle);
        stcpnet.sockets.prune();
    }
}

impl StcpStream {
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let &(ref stcpnetref, ref cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        while {
            let socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
            if !socket.may_recv() {
                return Ok(0);
            }
            !socket.can_recv()
        } {
            cond.wait(&mut stcpnet);
        }

        let r;
        {
            let mut socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
            r = socket
                .recv_slice(buf)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()));
        }
        if stcpnet.bg_skip_one_wait == Skip::Wait
            && (stcpnet.current_wait_delay == -1 as c_int
                || stcpnet.current_wait_delay > 10 as c_int)
        {
            let _ = stcpnet.notify_poll.send(b"$").unwrap();
        }
        r
    }

    pub fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let &(ref stcpnetref, ref cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        while {
            let socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
            if !socket.may_send() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "not connected (anymore)",
                ));
            }
            !socket.can_send()
        } {
            cond.wait(&mut stcpnet);
        }

        let r;
        {
            let mut socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
            r = socket
                .send_slice(buf)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()));
        }
        if stcpnet.bg_skip_one_wait == Skip::Wait {
            let _ = stcpnet.notify_poll.send(b"$").unwrap();
        } else {
            stcpnet.bg_skip_one_wait = Skip::Skip;
        }
        r
    }
    pub fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        let socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
        let local = socket.local_endpoint();
        Ok(endpoint_to_socket_addr(&local))
    }
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        let socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
        let peer = socket.remote_endpoint();
        if !peer.is_specified() {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "not connected (anymore)",
            ))
        } else {
            Ok(endpoint_to_socket_addr(&peer))
        }
    }
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        let mut socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
        match how {
            Shutdown::Read => {
                error!("TcpStream.shutdown(Shutdown::Read) not implemented");
                Ok(())
            }
            Shutdown::Write => {
                socket.close();
                Ok(())
            }
            Shutdown::Both => {
                socket.abort();
                Ok(())
            }
        }
    }
    pub fn try_clone(&self) -> io::Result<StcpStream> {
        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        stcpnet.sockets.retain(self.sockethandle); // cloning needs to increase the ref count
        Ok(StcpStream {
            stcpnet: self.stcpnet.clone(),
            sockethandle: self.sockethandle.clone(),
        })
    }
}

fn endpoint_to_socket_addr(ep: &IpEndpoint) -> SocketAddr {
    let stip = match ep.addr {
        IpAddress::Ipv4(v) => IpAddr::V4(Ipv4Addr::from(v)),
        IpAddress::Ipv6(v) => IpAddr::V6(Ipv6Addr::from(v)),
        IpAddress::Unspecified => IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        IpAddress::__Nonexhaustive => panic!("not covered"),
    };
    SocketAddr::new(stip, ep.port)
}
