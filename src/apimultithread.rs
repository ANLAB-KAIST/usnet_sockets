/// multithread (and default) version of the usnet_sockets API, uses a background thread
/// This API aims for compatibility with the socket types TcpStream and TcpListener
/// of the Rust standard library.
/// In contrast to the singlethread version, the socket types here can be used by a
/// multithreading application because they are movable and have locking.
/// Since there is a background thread which handles the socket timeouts and incoming
/// packets, the application may do longer computations without calling socket operations.
use parking_lot::{Condvar, Mutex, RwLock};
use smoltcp::time::Instant;
use std::io::{self, Write};
use std::iter;
use std::net::{SocketAddrV4, SocketAddrV6};
use std::option;
use std::os::unix::io::{AsRawFd, RawFd};
use std::slice;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant as StdInstant};
use std::vec;

use resolve;

use smoltcp;
use smoltcp::socket::{
    SocketHandle, SocketSet, TcpSocket, TcpSocketBuffer, UdpPacketMetadata,
    UdpSocket as SmoltcpUdpSocket, UdpSocketBuffer,
};
use smoltcp::wire::{IpAddress, IpEndpoint, IpProtocol};

use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener as SystemTcpListener,
    TcpStream as SystemTcpStream, UdpSocket as SystemUdpSocket,
};
use std::os::unix::net::UnixDatagram;

use nix::poll::{poll, EventFlags, PollFd};
use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::Pid;
use std::os::raw::c_int;

use device::*;
use rand::{thread_rng, Rng};
use std::env;
use std::io::prelude::*;
use system::read_kernel_local_port_range;
use usnetconfig::*;

use serde_json;

#[derive(Clone, Debug)]
pub struct StcpNetRef {
    pub r: Arc<(Mutex<StcpNet>, Condvar)>,
}

#[derive(Debug)]
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

#[derive(Debug)]
pub struct TcpListener {}

impl TcpListener {
    pub fn bind<A: UsnetToSocketAddrs>(addr: A) -> io::Result<StcpListenerRef> {
        STCP_GLOBAL.bind(addr)
    }
}

#[derive(PartialEq, Eq, Debug)]
enum Skip {
    Wait,
    Skippable,
    Skip,
}

#[derive(Debug)]
pub struct StcpNet {
    sockets: SocketSet<'static>,
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
    fn bind<A: UsnetToSocketAddrs>(&self, addr: A) -> io::Result<StcpListenerRef> {
        let mut r = Err(io::Error::new(
            io::ErrorKind::Other,
            "to_socket_addrs is empty",
        ));
        for mut sockaddr in addr.usnet_to_socket_addrs()? {
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
                            ttl: None,
                            nonblocking: false,
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
                    ttl: None,
                    nonblocking: false,
                })),
            });
        }
        r
    }
    fn connect_timeout(&self, addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        self.connect_timeout_opt(addr, Some(timeout))
    }
    fn connect<A: UsnetToSocketAddrs>(&self, addr: A) -> io::Result<TcpStream> {
        self.connect_timeout_opt(addr, None)
    }
    fn connect_timeout_opt<A: UsnetToSocketAddrs>(
        &self,
        addr: A,
        timeout: Option<Duration>,
    ) -> io::Result<TcpStream> {
        let mut r = Err(io::Error::new(
            io::ErrorKind::Other,
            "to_socket_addrs is empty",
        ));
        for addr in addr.usnet_to_socket_addrs()? {
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
                    r = match timeout {
                        Some(timeout) => SystemTcpStream::connect_timeout(&addr, timeout),
                        None => SystemTcpStream::connect(addr),
                    }
                    .map(|s| TcpStream::System(s));
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
                        _ => panic!("not covered"),
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

                let start = StdInstant::now();
                while !{
                    let socket = stcpnet.sockets.get::<TcpSocket>(tcp_handle);
                    if !socket.is_active() {
                        error = true;
                        true // break
                    } else {
                        socket.may_recv() && socket.may_send()
                    }
                } {
                    match timeout {
                        Some(timeout) => {
                            if cond.wait_until(&mut stcpnet, start + timeout).timed_out() {
                                error = true;
                                let mut socket = stcpnet.sockets.get::<TcpSocket>(tcp_handle);
                                socket.abort();
                                break;
                            }
                        }
                        None => {
                            cond.wait(&mut stcpnet);
                        }
                    }
                }
                if error {
                    stcpnet.sockets.release(tcp_handle);
                    stcpnet.sockets.prune();
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
                    nonblocking: Arc::new(AtomicBool::new(false)),
                    read_timeout: Arc::new(RwLock::new(None)),
                    write_timeout: Arc::new(RwLock::new(None)),
                }));
            }
        }
        r
    }
    fn bind_udp<A: UsnetToSocketAddrs>(&self, addr: A) -> io::Result<UdpSocket> {
        let mut r = Err(io::Error::new(
            io::ErrorKind::Other,
            "to_socket_addrs is empty",
        ));
        for mut sockaddr in addr.usnet_to_socket_addrs()? {
            let ipa = sockaddr.ip();
            if !ipa.is_loopback() && ipa.is_ipv6() {
                r = Err(io::Error::new(io::ErrorKind::Other, "ipv6 bind"));
                continue;
            }

            let mut listen_handle;
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
                    lo_result = SystemUdpSocket::bind(loaddr);
                    if lo_result.is_ok() && !sockaddr.ip().is_loopback() {
                        usnet_result = stcpnet.iface.add_port_match(
                            ipa,
                            Some(sockaddr.port()),
                            None,
                            None,
                            IpProtocol::Udp,
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
                    return Ok(UdpSocket {
                        stcpnet: (*self).clone(),
                        socket_handle: None,
                        lo: lolisten,
                        connected: Arc::new(RwLock::new(None)),
                        nonblocking: Arc::new(AtomicBool::new(false)),
                        read_timeout: Arc::new(RwLock::new(None)),
                        write_timeout: Arc::new(RwLock::new(None)),
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

                let udp_rx_buffer = UdpSocketBuffer::new(
                    vec![UdpPacketMetadata::EMPTY; 1000],
                    vec![0; stcpnet.socket_buffer_size],
                );
                let udp_tx_buffer = UdpSocketBuffer::new(
                    vec![UdpPacketMetadata::EMPTY; 1000],
                    vec![0; stcpnet.socket_buffer_size],
                );
                let udp_socket = SmoltcpUdpSocket::new(udp_rx_buffer, udp_tx_buffer);

                listen_handle = stcpnet.sockets.add(udp_socket);
                let mut socket = stcpnet.sockets.get::<SmoltcpUdpSocket>(listen_handle);
                socket.bind(sockaddr.port()).unwrap();
            }
            return Ok(UdpSocket {
                stcpnet: (*self).clone(),
                socket_handle: Some(listen_handle),
                lo: lolisten,
                connected: Arc::new(RwLock::new(None)),
                nonblocking: Arc::new(AtomicBool::new(false)),
                read_timeout: Arc::new(RwLock::new(None)),
                write_timeout: Arc::new(RwLock::new(None)),
            });
        }
        r
    }
}

#[derive(Debug)]
pub struct StcpListener {
    stcpnet: StcpNetRef,
    listen_handles: Option<Vec<SocketHandle>>,
    lo: SystemTcpListener,
    port: u16,
    ttl: Option<u8>,
    nonblocking: bool,
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

#[derive(Debug)]
pub struct StcpStream {
    stcpnet: StcpNetRef,
    sockethandle: SocketHandle,
    nonblocking: Arc<AtomicBool>,
    read_timeout: Arc<RwLock<Option<Duration>>>,
    write_timeout: Arc<RwLock<Option<Duration>>>,
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
    pub fn ttl(&self) -> io::Result<u32> {
        let listener = self.l.lock();
        Ok(listener.ttl.unwrap_or(64) as u32)
    }
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        let mut listener = self.l.lock();
        listener.ttl = Some(ttl as u8);
        if let Some(handles) = listener.listen_handles.as_ref() {
            let &(ref stcpnetref, ref _cond) = &*listener.stcpnet.r;
            let mut stcpnet = stcpnetref.lock();
            for handle in handles.iter() {
                let mut socket = stcpnet.sockets.get::<TcpSocket>(*handle);
                socket.set_hop_limit(listener.ttl);
            }
        }
        listener.lo.set_ttl(ttl)
    }
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        let listener = self.l.lock();
        listener.lo.take_error()
    }
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let mut listener = self.l.lock();
        listener.nonblocking = nonblocking;
        if listener.listen_handles.is_none() {
            // only loopback, otherwise always nonblocking
            listener.lo.set_nonblocking(nonblocking)?;
        }
        Ok(())
    }
    pub fn try_clone(&self) -> io::Result<StcpListenerRef> {
        Ok(StcpListenerRef { l: self.l.clone() })
    }
    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let mut listener = self.l.lock();
        if listener.listen_handles.is_none() {
            return listener.lo.accept().map(|(s, a)| {
                s.set_nonblocking(listener.nonblocking)
                    .expect("couldn't set nonblocking option");
                (TcpStream::System(s), a)
            });
        }
        loop {
            let mut r = None;
            {
                let &(ref stcpnetref, ref cond) = &*listener.stcpnet.r;
                let mut stcpnet = stcpnetref.lock();

                if !listener.nonblocking {
                    cond.wait(&mut stcpnet);
                }

                let sys_res = listener.lo.accept();
                if let Ok((s, a)) = sys_res {
                    s.set_nonblocking(listener.nonblocking)
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
                                    nonblocking: Arc::new(AtomicBool::new(listener.nonblocking)),
                                    read_timeout: Arc::new(RwLock::new(None)),
                                    write_timeout: Arc::new(RwLock::new(None)),
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
                        socket.set_hop_limit(listener.ttl);
                        socket.listen(listener.port).unwrap();
                    }
                    listener.listen_handles.as_mut().unwrap().push(tcp_handle);
                    return r;
                }
                _ => {}
            }
            if listener.nonblocking {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "no active connection found",
                ));
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
    pub fn connect<A: UsnetToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        STCP_GLOBAL.connect(addr)
    }
    pub fn connect_timeout(addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        STCP_GLOBAL.connect_timeout(addr, timeout)
    }
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match self {
            TcpStream::Stcp(slf) => slf.set_read_timeout(dur),
            TcpStream::System(slf) => slf.set_read_timeout(dur),
        }
    }
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        match self {
            TcpStream::Stcp(slf) => slf.read_timeout(),
            TcpStream::System(slf) => slf.read_timeout(),
        }
    }
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match self {
            TcpStream::Stcp(slf) => slf.set_write_timeout(dur),
            TcpStream::System(slf) => slf.set_write_timeout(dur),
        }
    }
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        match self {
            TcpStream::Stcp(slf) => slf.write_timeout(),
            TcpStream::System(slf) => slf.write_timeout(),
        }
    }
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TcpStream::Stcp(slf) => slf.peek(buf),
            TcpStream::System(slf) => slf.peek(buf),
        }
    }
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        match self {
            TcpStream::Stcp(slf) => slf.set_nodelay(nodelay),
            TcpStream::System(slf) => slf.set_nodelay(nodelay),
        }
    }
    pub fn nodelay(&self) -> io::Result<bool> {
        match self {
            TcpStream::Stcp(slf) => slf.nodelay(),
            TcpStream::System(slf) => slf.nodelay(),
        }
    }
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        match self {
            TcpStream::Stcp(slf) => slf.set_ttl(ttl),
            TcpStream::System(slf) => slf.set_ttl(ttl),
        }
    }
    pub fn ttl(&self) -> io::Result<u32> {
        match self {
            TcpStream::Stcp(slf) => slf.ttl(),
            TcpStream::System(slf) => slf.ttl(),
        }
    }
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        match self {
            TcpStream::Stcp(_) => Ok(None),
            TcpStream::System(slf) => slf.take_error(),
        }
    }
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            TcpStream::Stcp(slf) => slf.set_nonblocking(nonblocking),
            TcpStream::System(slf) => slf.set_nonblocking(nonblocking),
        }
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
                let start = StdInstant::now();
                let &(ref stcpnetref, ref cond) = &*slf.stcpnet.r;
                let mut stcpnet = stcpnetref.lock();

                while {
                    let socket = stcpnet.sockets.get::<TcpSocket>(slf.sockethandle);
                    if !socket.may_send() {
                        return Err(smoltcp::Error::Illegal);
                    }
                    !socket.can_send()
                } {
                    if slf.nonblocking.load(Ordering::SeqCst) {
                        return Err(smoltcp::Error::Exhausted);
                    }
                    match *slf.write_timeout.read() {
                        Some(timeout) => {
                            if cond.wait_until(&mut stcpnet, start + timeout).timed_out() {
                                return Err(smoltcp::Error::Exhausted);
                            }
                        }
                        None => {
                            cond.wait(&mut stcpnet);
                        }
                    }
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
                let start = StdInstant::now();
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
                    if slf.nonblocking.load(Ordering::SeqCst) {
                        return Err(smoltcp::Error::Exhausted);
                    }
                    match *slf.read_timeout.read() {
                        Some(timeout) => {
                            if cond.wait_until(&mut stcpnet, start + timeout).timed_out() {
                                return Err(smoltcp::Error::Exhausted);
                            }
                        }
                        None => {
                            cond.wait(&mut stcpnet);
                        }
                    }
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
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        let start = StdInstant::now();
        let &(ref stcpnetref, ref cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        while {
            let socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
            if !socket.may_recv() {
                return Ok(0);
            }
            !socket.can_recv()
        } {
            if self.nonblocking.load(Ordering::SeqCst) {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "peek not ready"));
            }
            match *self.read_timeout.read() {
                Some(timeout) => {
                    if cond.wait_until(&mut stcpnet, start + timeout).timed_out() {
                        return Err(io::Error::new(io::ErrorKind::WouldBlock, "peek not ready"));
                    }
                }
                None => {
                    cond.wait(&mut stcpnet);
                }
            }
        }

        {
            let mut socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
            socket
                .peek_slice(buf)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
        }
    }
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let start = StdInstant::now();
        let &(ref stcpnetref, ref cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        while {
            let socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
            if !socket.may_recv() {
                return Ok(0);
            }
            !socket.can_recv()
        } {
            if self.nonblocking.load(Ordering::SeqCst) {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "read not ready"));
            }
            match *self.read_timeout.read() {
                Some(timeout) => {
                    if cond.wait_until(&mut stcpnet, start + timeout).timed_out() {
                        return Err(io::Error::new(io::ErrorKind::WouldBlock, "read not ready"));
                    }
                }
                None => {
                    cond.wait(&mut stcpnet);
                }
            }
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
        let start = StdInstant::now();
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
            if self.nonblocking.load(Ordering::SeqCst) {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "write not ready"));
            }
            match *self.write_timeout.read() {
                Some(timeout) => {
                    if cond.wait_until(&mut stcpnet, start + timeout).timed_out() {
                        return Err(io::Error::new(io::ErrorKind::WouldBlock, "write not ready"));
                    }
                }
                None => {
                    cond.wait(&mut stcpnet);
                }
            }
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
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        *self.read_timeout.write() = dur;
        Ok(())
    }
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(*self.read_timeout.read())
    }
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        *self.write_timeout.write() = dur;
        Ok(())
    }
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(*self.write_timeout.read())
    }
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        if nodelay {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "smoltcp does not have Nagle's algorithm",
            ))
        }
    }
    pub fn nodelay(&self) -> io::Result<bool> {
        Ok(true)
    }
    pub fn flush(&mut self) -> io::Result<()> {
        Ok(()) // because of nodelay
    }
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.nonblocking.store(nonblocking, Ordering::SeqCst);
        Ok(())
    }
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        let mut socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
        socket.set_hop_limit(Some(ttl as u8));
        Ok(())
    }
    pub fn ttl(&self) -> io::Result<u32> {
        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        let socket = stcpnet.sockets.get::<TcpSocket>(self.sockethandle);
        Ok(socket.hop_limit().unwrap_or(64) as u32)
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
            nonblocking: self.nonblocking.clone(),
            read_timeout: self.read_timeout.clone(),
            write_timeout: self.write_timeout.clone(),
        })
    }
}

fn endpoint_to_socket_addr(ep: &IpEndpoint) -> SocketAddr {
    let stip = match ep.addr {
        IpAddress::Ipv4(v) => IpAddr::V4(Ipv4Addr::from(v)),
        IpAddress::Ipv6(v) => IpAddr::V6(Ipv6Addr::from(v)),
        IpAddress::Unspecified => IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        _ => panic!("not covered"),
    };
    SocketAddr::new(stip, ep.port)
}

// UDP
#[derive(Debug)]
pub struct UdpSocket {
    stcpnet: StcpNetRef,
    socket_handle: Option<SocketHandle>,
    lo: SystemUdpSocket,
    connected: Arc<RwLock<Option<SocketAddr>>>,
    nonblocking: Arc<AtomicBool>,
    read_timeout: Arc<RwLock<Option<Duration>>>,
    write_timeout: Arc<RwLock<Option<Duration>>>,
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        stcpnet.fds_remove.push(self.lo.as_raw_fd());
        if let Some(sockethandle) = self.socket_handle {
            debug!("drop-closing UDP");
            stcpnet.sockets.release(sockethandle);
            stcpnet.sockets.prune();
        }
    }
}

impl UdpSocket {
    pub fn bind<A: UsnetToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        STCP_GLOBAL.bind_udp(addr)
    }
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.peek_or_recv_from(true, buf)
    }
    fn peek_or_recv_from(&self, recv: bool, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        if self.socket_handle.is_none()
            || match *self.connected.read() {
                Some(sa) => sa.ip().is_loopback(),
                None => false,
            }
        {
            return if recv {
                self.lo.recv_from(buf)
            } else {
                self.lo.peek_from(buf)
            };
        }
        let socket_handle = self.socket_handle.unwrap();

        let start = StdInstant::now();
        let &(ref stcpnetref, ref cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        loop {
            {
                let mut socket = stcpnet.sockets.get::<SmoltcpUdpSocket>(socket_handle);
                if socket.can_recv() {
                    let r = if recv {
                        socket.recv_slice(buf)
                    } else {
                        socket.peek_slice(buf).map(|(r, e)| (r, e.clone()))
                    };
                    let r = r
                        .map(|(read, addr)| (read, endpoint_to_socket_addr(&addr)))
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()));
                    if let Some(ref conn_remote_addr) = *self.connected.read() {
                        match &r {
                            Ok((_, sockaddr)) => {
                                if sockaddr != conn_remote_addr
                                    && !(conn_remote_addr.ip().is_unspecified()
                                        && conn_remote_addr.port() == 0)
                                    && !(conn_remote_addr.ip().is_unspecified()
                                        && sockaddr.port() == conn_remote_addr.port())
                                    && !(conn_remote_addr.port() == 0
                                        && conn_remote_addr.ip() == sockaddr.ip())
                                {
                                    continue;
                                }
                            }
                            Err(_) => {}
                        }
                    }
                    return r;
                }
            }
            if (*self.connected.read()).is_none() {
                let sys_res = if recv {
                    self.lo.recv_from(buf)
                } else {
                    self.lo.peek_from(buf)
                };
                if sys_res
                    .as_ref()
                    .err()
                    .map(|e| e.kind() == io::ErrorKind::WouldBlock)
                    != Some(true)
                {
                    return sys_res;
                }
            }
            if self.nonblocking.load(Ordering::SeqCst) {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "recv not ready"));
            }
            match *self.read_timeout.read() {
                Some(timeout) => {
                    if cond.wait_until(&mut stcpnet, start + timeout).timed_out() {
                        return Err(io::Error::new(io::ErrorKind::WouldBlock, "recv not ready"));
                    }
                }
                None => {
                    cond.wait(&mut stcpnet);
                }
            }
        }
    }
    pub fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.peek_or_recv_from(false, buf)
    }
    pub fn send_to<A: UsnetToSocketAddrs>(&self, buf: &[u8], addr: A) -> io::Result<usize> {
        let addr = addr
            .usnet_to_socket_addrs()?
            .next()
            .ok_or(io::Error::new(io::ErrorKind::Other, "to sock addr empty"))?;
        if self.socket_handle.is_none() || addr.ip().is_loopback() {
            if self.socket_handle.is_some() && !self.nonblocking.load(Ordering::SeqCst) {
                self.lo.set_nonblocking(false)?;
            }
            let r = self.lo.send_to(buf, addr);
            if self.socket_handle.is_some() && !self.nonblocking.load(Ordering::SeqCst) {
                self.set_nonblocking(self.nonblocking.load(Ordering::SeqCst))?;
            }
            return r;
        }
        let socket_handle = self.socket_handle.unwrap();

        let start = StdInstant::now();
        let &(ref stcpnetref, ref cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        loop {
            let mut r = None;
            {
                let mut socket = stcpnet.sockets.get::<SmoltcpUdpSocket>(socket_handle);
                if socket.can_send() {
                    r = Some(
                        socket
                            .send_slice(buf, addr.into())
                            .map(|_| buf.len())
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string())),
                    );
                }
            }
            if let Some(r) = r {
                if stcpnet.bg_skip_one_wait == Skip::Wait {
                    let _ = stcpnet.notify_poll.send(b"$").unwrap();
                } else {
                    stcpnet.bg_skip_one_wait = Skip::Skip;
                }
                return r;
            }
            if self.nonblocking.load(Ordering::SeqCst) {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "send not ready"));
            }
            match *self.read_timeout.read() {
                Some(timeout) => {
                    if cond.wait_until(&mut stcpnet, start + timeout).timed_out() {
                        return Err(io::Error::new(io::ErrorKind::WouldBlock, "send not ready"));
                    }
                }
                None => {
                    cond.wait(&mut stcpnet);
                }
            }
        }
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self.socket_handle {
            None => {
                return self.lo.local_addr();
            }
            Some(ref handle) => {
                let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
                let mut stcpnet = stcpnetref.lock();
                let socket = stcpnet.sockets.get::<SmoltcpUdpSocket>(*handle);
                let local = socket.endpoint();
                Ok(endpoint_to_socket_addr(&local))
            }
        }
    }
    pub fn try_clone(&self) -> io::Result<UdpSocket> {
        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let mut stcpnet = stcpnetref.lock();
        if let Some(socket_handle) = self.socket_handle {
            stcpnet.sockets.retain(socket_handle); // cloning needs to increase the ref count
        }
        Ok(UdpSocket {
            stcpnet: self.stcpnet.clone(),
            socket_handle: self.socket_handle.clone(),
            lo: self.lo.try_clone()?,
            connected: self.connected.clone(),
            nonblocking: self.nonblocking.clone(),
            read_timeout: self.read_timeout.clone(),
            write_timeout: self.write_timeout.clone(),
        })
    }
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        *self.read_timeout.write() = dur;
        self.lo.set_read_timeout(*self.read_timeout.read())
    }
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        *self.write_timeout.write() = dur;
        self.lo.set_write_timeout(*self.write_timeout.read())
    }
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(*self.read_timeout.read())
    }
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(*self.write_timeout.read())
    }
    pub fn set_broadcast(&self, broadcast: bool) -> io::Result<()> {
        panic!("unimpl")
    }
    pub fn broadcast(&self) -> io::Result<bool> {
        panic!("unimpl")
    }
    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> io::Result<()> {
        panic!("unimpl")
    }
    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        panic!("unimpl")
    }
    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> io::Result<()> {
        panic!("unimpl")
    }
    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        panic!("unimpl")
    }
    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> io::Result<()> {
        panic!("unimpl")
    }
    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        panic!("unimpl")
    }
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        if let Some(socket_handle) = self.socket_handle {
            let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
            let mut stcpnet = stcpnetref.lock();
            let mut socket = stcpnet.sockets.get::<SmoltcpUdpSocket>(socket_handle);
            socket.set_hop_limit(Some(ttl as u8));
        }
        self.lo.set_ttl(ttl)
    }
    pub fn ttl(&self) -> io::Result<u32> {
        self.lo.ttl()
    }
    pub fn join_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> io::Result<()> {
        panic!("unimpl")
    }
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        panic!("unimpl")
    }
    pub fn leave_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> io::Result<()> {
        panic!("unimpl")
    }
    pub fn leave_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        panic!("unimpl")
    }
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.lo.take_error()
    }
    pub fn connect<A: UsnetToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addr = addr
            .usnet_to_socket_addrs()?
            .next()
            .ok_or(io::Error::new(io::ErrorKind::Other, "to sock addr empty"))?;
        if !self.nonblocking.load(Ordering::SeqCst) {
            self.lo.set_nonblocking(addr.ip().is_loopback())?;
        }

        let &(ref stcpnetref, ref _cond) = &*self.stcpnet.r;
        let stcpnet = stcpnetref.lock();
        let ipv4 = stcpnet.iface.ips()[0].address();
        if addr.ip().is_loopback() || IpAddress::from(addr.ip()) == ipv4 {
            *self.connected.write() = Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                addr.port(),
            ));
            self.lo.connect(addr)
        } else {
            *self.connected.write() = Some(addr.clone());
            Ok(())
        }
    }
    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        match *self.connected.read() {
            Some(c) => self.send_to(buf, c),
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "not connected")),
        }
    }
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        if (*self.connected.read()).is_none() {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "not connected"))
        } else {
            self.recv_from(buf).map(|(r, _)| r)
        }
    }
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        if (*self.connected.read()).is_none() {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "not connected"))
        } else {
            self.peek_from(buf).map(|(r, _)| r)
        }
    }
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match *self.connected.read() {
            Some(addr) => {
                self.lo.set_nonblocking(if !nonblocking {
                    !addr.ip().is_loopback()
                } else {
                    true
                })?;
            }
            None => {
                self.lo.set_nonblocking(if self.socket_handle.is_some() {
                    true
                } else {
                    nonblocking
                })?;
            }
        }
        self.nonblocking.store(nonblocking, Ordering::SeqCst);
        Ok(())
    }
}

// unfortunately need to port Rust std lib type from /src/std/net/addr.rs (copyright MIT/Apache 2.0, see https://thanks.rust-lang.org)
pub trait UsnetToSocketAddrs {
    type Iter: Iterator<Item = SocketAddr>;
    fn usnet_to_socket_addrs(&self) -> io::Result<Self::Iter>;
}

fn usnet_resolve_socket_addr(s: &str, p: u16) -> io::Result<vec::IntoIter<SocketAddr>> {
    if let Ok(hostname) = resolve::hostname::get_hostname() {
        if hostname == s {
            return Ok(
                vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), p)].into_iter(),
            );
        }
    }
    if let Ok(etc_hosts) = resolve::hosts::load_hosts(&resolve::hosts::host_file()) {
        if let Some(addr) = etc_hosts.find_address(s) {
            return Ok(vec![SocketAddr::new(addr, p)].into_iter());
        }
    }
    let config = resolve::DnsConfig::with_name_servers(vec![
        "1.1.1.1:53".parse().unwrap(),
        "1.0.0.1:53".parse().unwrap(),
    ]);
    let resolver = resolve::DnsResolver::new(config)?;
    let ips = resolver.resolve_host(s)?;
    let v: Vec<_> = ips.map(|a| SocketAddr::new(a, p)).collect();
    Ok(v.into_iter())
}

impl UsnetToSocketAddrs for String {
    type Iter = vec::IntoIter<SocketAddr>;
    fn usnet_to_socket_addrs(&self) -> io::Result<vec::IntoIter<SocketAddr>> {
        (&**self).usnet_to_socket_addrs()
    }
}

impl<'a, T: UsnetToSocketAddrs + ?Sized> UsnetToSocketAddrs for &'a T {
    type Iter = T::Iter;
    fn usnet_to_socket_addrs(&self) -> io::Result<T::Iter> {
        (**self).usnet_to_socket_addrs()
    }
}
impl<'a> UsnetToSocketAddrs for &'a [SocketAddr] {
    type Iter = iter::Cloned<slice::Iter<'a, SocketAddr>>;

    fn usnet_to_socket_addrs(&self) -> io::Result<Self::Iter> {
        Ok(self.iter().cloned())
    }
}
impl<'a> UsnetToSocketAddrs for (&'a str, u16) {
    type Iter = vec::IntoIter<SocketAddr>;
    fn usnet_to_socket_addrs(&self) -> io::Result<vec::IntoIter<SocketAddr>> {
        let (host, port) = *self;

        // try to parse the host as a regular IP address first
        if let Ok(addr) = host.parse::<Ipv4Addr>() {
            let addr = SocketAddrV4::new(addr, port);
            return Ok(vec![SocketAddr::V4(addr)].into_iter());
        }
        if let Ok(addr) = host.parse::<Ipv6Addr>() {
            let addr = SocketAddrV6::new(addr, port, 0, 0);
            return Ok(vec![SocketAddr::V6(addr)].into_iter());
        }

        usnet_resolve_socket_addr(host, port)
    }
}
impl UsnetToSocketAddrs for (IpAddr, u16) {
    type Iter = option::IntoIter<SocketAddr>;
    fn usnet_to_socket_addrs(&self) -> io::Result<option::IntoIter<SocketAddr>> {
        let (ip, port) = *self;
        match ip {
            IpAddr::V4(ref a) => (*a, port).usnet_to_socket_addrs(),
            IpAddr::V6(ref a) => (*a, port).usnet_to_socket_addrs(),
        }
    }
}
impl UsnetToSocketAddrs for SocketAddrV6 {
    type Iter = option::IntoIter<SocketAddr>;
    fn usnet_to_socket_addrs(&self) -> io::Result<option::IntoIter<SocketAddr>> {
        SocketAddr::V6(*self).usnet_to_socket_addrs()
    }
}
impl UsnetToSocketAddrs for SocketAddrV4 {
    type Iter = option::IntoIter<SocketAddr>;
    fn usnet_to_socket_addrs(&self) -> io::Result<option::IntoIter<SocketAddr>> {
        SocketAddr::V4(*self).usnet_to_socket_addrs()
    }
}
impl UsnetToSocketAddrs for SocketAddr {
    type Iter = option::IntoIter<SocketAddr>;
    fn usnet_to_socket_addrs(&self) -> io::Result<option::IntoIter<SocketAddr>> {
        Ok(Some(*self).into_iter())
    }
}
impl UsnetToSocketAddrs for (Ipv6Addr, u16) {
    type Iter = option::IntoIter<SocketAddr>;
    fn usnet_to_socket_addrs(&self) -> io::Result<option::IntoIter<SocketAddr>> {
        let (ip, port) = *self;
        SocketAddrV6::new(ip, port, 0, 0).usnet_to_socket_addrs()
    }
}
impl UsnetToSocketAddrs for (Ipv4Addr, u16) {
    type Iter = option::IntoIter<SocketAddr>;
    fn usnet_to_socket_addrs(&self) -> io::Result<option::IntoIter<SocketAddr>> {
        let (ip, port) = *self;
        SocketAddrV4::new(ip, port).usnet_to_socket_addrs()
    }
}
impl UsnetToSocketAddrs for str {
    type Iter = vec::IntoIter<SocketAddr>;
    fn usnet_to_socket_addrs(&self) -> io::Result<vec::IntoIter<SocketAddr>> {
        // try to parse as a regular SocketAddr first
        if let Some(addr) = self.parse().ok() {
            return Ok(vec![addr].into_iter());
        }

        macro_rules! try_opt {
            ($e:expr, $msg:expr) => {
                match $e {
                    Some(r) => r,
                    None => return Err(io::Error::new(io::ErrorKind::InvalidInput, $msg)),
                }
            };
        }

        // split the string by ':' and convert the second part to u16
        let mut parts_iter = self.rsplitn(2, ':');
        let port_str = try_opt!(parts_iter.next(), "invalid socket address");
        let host = try_opt!(parts_iter.next(), "invalid socket address");
        let port: u16 = try_opt!(port_str.parse().ok(), "invalid port value");
        usnet_resolve_socket_addr(host, port)
    }
}
// port of /src/std/net/addr.rs ends here (copyright MIT/Apache 2.0, see https://thanks.rust-lang.org)
