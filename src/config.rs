use std::collections::BTreeMap;
use std::fmt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::str::{self, FromStr};

use std::fs::remove_file;
use std::process;

extern crate libusnetd;
use self::libusnetd::{ClientMessage, ClientMessageIp, SOCKET_PATH};
extern crate usnet_devices;
#[cfg(feature = "netmap")]
use self::usnet_devices::{nmreq, Netmap};

use self::usnet_devices::{RawSocket, TapInterface, UnixDomainSocket};

use smoltcp::iface::{EthernetInterfaceBuilder, NeighborCache, Routes};
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpProtocol, Ipv4Address};

use std::os::unix::net::UnixDatagram;

use libc;
use nix::sys::socket::{recvmsg, CmsgSpace, ControlMessage, MsgFlags, SockAddr};
use nix::sys::uio::IoVec;
use nix::unistd::{gettid, getuid};
use std::os::unix::io::FromRawFd;

use device::*;
use std::fs;
use std::io::prelude::*;
use std::process::Command;
use system::*;

#[cfg(feature = "netmap")]
use std::mem;

use rand;

use serde_json;

#[derive(Debug, Serialize, Deserialize)]
pub enum MacVtapDevice {
    Create {
        mac: Mac,
        parent: SystemInterface,
        ipv4: IpV4,
    },
    Interface {
        interface: String,
        ipv4: IpV4,
    }, // copies MAC from device, must have been set after creation
}

#[derive(Debug, Serialize, Deserialize)]
pub enum SystemInterface {
    DiscoverFromRoute,
    Interface(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Mac {
    Passthru, // copy from parent, takes ownership of device, kernel does not process traffic anymore
    Static(String),
    Random,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum IpV4 {
    Dhcp,
    Static {
        ipv4: String,
        sub: u8,
        gateway: String,
    },
    Passthru, // copy from parent
}

#[derive(Debug, Serialize, Deserialize)]
pub enum TapDevice {
    Create {
        mac: TapMac,
        ipv4: TapIpV4,
        host_ip: String,
    },
    Interface {
        interface: String,
        mac: TapMac,
        ipv4: TapIpV4,
    }, // MAC and IP can be chosen as wanted but should be distinct from the host IP
}

#[derive(Debug, Serialize, Deserialize)]
pub enum TapIpV4 {
    Static {
        ipv4: String,
        sub: u8,
        gateway: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum TapMac {
    Static(String),
    Random,
}

#[cfg(feature = "netmap")]
#[derive(Debug, Serialize, Deserialize)]
pub enum NetmapInterface {
    DiscoverFromRoute,
    Interface { netmap_name: String, parent: String },
}

#[cfg(feature = "netmap")]
#[derive(Debug, Serialize, Deserialize)]
pub enum NetmapDevice {
    Interface {
        interface: NetmapInterface,
        mac: Mac,
        ipv4: IpV4,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum UsnetType {
    #[cfg(feature = "netmap")]
    NetmapPipe,
    UnixDomainSocket,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum UsnetDevice {
    Interface {
        interface: SystemInterface,
        ipc: UsnetType,
        mac: Mac,
        ipv4: IpV4,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum StcpBackend {
    RawConfig,
    TapConfig(TapDevice),
    MacVtapConfig(MacVtapDevice),
    #[cfg(feature = "netmap")]
    NetmapConfig(NetmapDevice),
    UsnetConfig(UsnetDevice),
}

impl fmt::Display for StcpBackend {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

impl StcpBackend {
    pub fn to_string(&self) -> String {
        serde_json::to_string(self).unwrap()
    }

    // this function needs refactoring for code sharing
    pub fn to_interface(
        self,
        waiting_poll: bool,
        reduce_mtu_by: Option<usize>,
    ) -> (RawFd, StcpBackendInterface) {
        let _ = waiting_poll;
        let neighbor_cache = NeighborCache::new(BTreeMap::new());
        match self {
            StcpBackend::RawConfig => {
                let coutput = Command::new("ip")
                    .args(&["route", "get", "1"])
                    .output()
                    .expect("command ip route get 1 failed");
                let sout1 = String::from_utf8(coutput.stdout).unwrap();
                let sout = sout1.split(' ').collect::<Vec<_>>();
                let ifname = sout[4];
                let gateway = sout[2];
                let ip = sout[6];
                debug!("ifname {}", ifname);
                let device = RawSocket::new(ifname, reduce_mtu_by).unwrap();
                let fd = device.as_raw_fd();
                let mut macaddr = String::new();
                {
                    let mut f = fs::File::open("/sys/class/net/".to_owned() + ifname + "/address")
                        .expect("invalid interface");
                    f.read_to_string(&mut macaddr)
                        .expect("can not read mac addr");
                }
                let m = macaddr
                    .trim()
                    .split(':')
                    .map(|s| u8::from_str_radix(s, 16).unwrap())
                    .collect::<Vec<_>>();
                let ethernet_addr = EthernetAddress([m[0], m[1], m[2], m[3], m[4], m[5]]);

                let ioutput = Command::new("ip")
                    .args(&["a", "show", "dev", ifname])
                    .output()
                    .expect("command ip show failed");
                let iout1 = String::from_utf8(ioutput.stdout).unwrap();
                let iout2 = iout1.split('/').collect::<Vec<_>>()[2];
                let iout = iout2.split(' ').collect::<Vec<_>>()[0];
                let sub = u8::from_str(iout).expect("net size not found");
                debug!("/{}", sub);

                let ip_addrs = [IpCidr::new(IpAddress::from_str(ip).expect("ip conv"), sub)];
                let default_v4_gw = Ipv4Address::from_str(gateway).expect("gateway conv");
                let mut routes = Routes::new(BTreeMap::new());
                routes.add_default_ipv4_route(default_v4_gw).unwrap();
                let mut iface = EthernetInterfaceBuilder::new(device)
                    .ethernet_addr(ethernet_addr)
                    .neighbor_cache(neighbor_cache)
                    .ip_addrs(ip_addrs)
                    .routes(routes)
                    .finalize();
                (fd, StcpBackendInterface::Raw(iface))
            }
            StcpBackend::MacVtapConfig(device) => {
                let (iface, fd, destroy) = match device {
                    MacVtapDevice::Create { mac, parent, ipv4 } => {
                        let origifname = match parent {
                            SystemInterface::DiscoverFromRoute => get_default_interface(),
                            SystemInterface::Interface(name) => name,
                        };
                        let (mode, ifname, macaddr): (String, String, String) = match mac {
                            Mac::Passthru => {
                                let ifname: String = origifname.clone() + "pass";
                                ("passthru".to_string(), ifname, get_macaddr(&origifname))
                            }
                            Mac::Static(mac_str) => {
                                let ifname: String = mac_str.replace(":", "");
                                ("bridge".to_string(), ifname, mac_str)
                            }
                            Mac::Random => {
                                let mac_str = gen_random_mac();
                                let ifname: String = mac_str.replace(":", "");
                                ("bridge".to_string(), ifname, mac_str)
                            }
                        };
                        let ethernet_addr = mac_str_to_eth_addr(&macaddr);
                        create_macvtap(&ifname, &origifname, &mode, &macaddr); // invokes pkexec
                        let device = TapInterface::new_macvtap(&ifname, reduce_mtu_by).unwrap();
                        let fd = device.as_raw_fd();

                        let mut iface = EthernetInterfaceBuilder::new(device)
                            .ethernet_addr(ethernet_addr)
                            .neighbor_cache(neighbor_cache);

                        let iface = match ipv4 {
                            IpV4::Dhcp => panic!("DHCP not implemented yet in smoltcp"),
                            IpV4::Passthru => {
                                let mut routes = Routes::new(BTreeMap::new());
                                if let Some(gw) = get_gateway(&origifname) {
                                    let gateway = ip_addr_from_str(&gw);
                                    routes.add_default_ipv4_route(gateway).unwrap();
                                }
                                let (ipv4, sub) = get_ipv4(&origifname);
                                let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                                iface.ip_addrs(ip_addrs).routes(routes)
                            }
                            IpV4::Static { ipv4, sub, gateway } => {
                                let gateway = ip_addr_from_str(&gateway);
                                let mut routes = Routes::new(BTreeMap::new());
                                routes.add_default_ipv4_route(gateway).unwrap();
                                let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                                iface.ip_addrs(ip_addrs).routes(routes)
                            }
                        };

                        (iface.finalize(), fd, Some(ifname))
                    }
                    MacVtapDevice::Interface { interface, ipv4 } => {
                        // copies MAC from device, must have been set after creation
                        let macaddr = get_macaddr(&interface);
                        let ethernet_addr = mac_str_to_eth_addr(&macaddr);

                        let device = TapInterface::new_macvtap(&interface, reduce_mtu_by).unwrap();
                        let fd = device.as_raw_fd();

                        let mut iface = EthernetInterfaceBuilder::new(device)
                            .ethernet_addr(ethernet_addr)
                            .neighbor_cache(neighbor_cache);

                        let iface = match ipv4 {
                            IpV4::Dhcp => panic!("DHCP not implemented yet in smoltcp"),
                            IpV4::Passthru => {
                                let origifname = get_macvtap_parent(&interface);
                                let mut routes = Routes::new(BTreeMap::new());
                                if let Some(gw) = get_gateway(&origifname) {
                                    let gateway = ip_addr_from_str(&gw);
                                    routes.add_default_ipv4_route(gateway).unwrap();
                                }
                                let (ipv4, sub) = get_ipv4(&origifname);
                                let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                                iface.ip_addrs(ip_addrs).routes(routes)
                            }
                            IpV4::Static { ipv4, sub, gateway } => {
                                let gateway = ip_addr_from_str(&gateway);
                                let mut routes = Routes::new(BTreeMap::new());
                                routes.add_default_ipv4_route(gateway).unwrap();
                                let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                                iface.ip_addrs(ip_addrs).routes(routes)
                            }
                        };

                        (iface.finalize(), fd, None)
                    }
                };

                (
                    fd,
                    StcpBackendInterface::MacVtap {
                        interface: iface,
                        destroy: destroy,
                    },
                )
            }
            StcpBackend::TapConfig(device) => {
                let (iface, fd, destroy) = match device {
                    TapDevice::Create { mac, ipv4, host_ip } => {
                        let macaddr: String = match mac {
                            TapMac::Static(mac_str) => mac_str,
                            TapMac::Random => gen_random_mac(),
                        };
                        let ethernet_addr = mac_str_to_eth_addr(&macaddr);

                        let (iface, fd, ifname) = match ipv4 {
                            TapIpV4::Static { ipv4, sub, gateway } => {
                                let ifname = ipv4.replace(".", "o"); // make interface name easy to recognize
                                create_tap(&ifname, &host_ip, sub); // invokes pkexec
                                let device = TapInterface::new(&ifname, reduce_mtu_by).unwrap();
                                let fd = device.as_raw_fd();

                                let mut iface = EthernetInterfaceBuilder::new(device)
                                    .ethernet_addr(ethernet_addr)
                                    .neighbor_cache(neighbor_cache);

                                let gateway = ip_addr_from_str(&gateway);
                                let mut routes = Routes::new(BTreeMap::new());
                                routes.add_default_ipv4_route(gateway).unwrap();
                                let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                                (iface.ip_addrs(ip_addrs).routes(routes), fd, ifname)
                            }
                        };

                        (iface.finalize(), fd, Some(ifname))
                    }
                    TapDevice::Interface {
                        interface,
                        mac,
                        ipv4,
                    } => {
                        let macaddr: String = match mac {
                            TapMac::Static(mac_str) => mac_str,
                            TapMac::Random => gen_random_mac(),
                        };
                        let ethernet_addr = mac_str_to_eth_addr(&macaddr);

                        let device = TapInterface::new(&interface, reduce_mtu_by).unwrap();
                        let fd = device.as_raw_fd();

                        let mut iface = EthernetInterfaceBuilder::new(device)
                            .ethernet_addr(ethernet_addr)
                            .neighbor_cache(neighbor_cache);

                        let iface = match ipv4 {
                            TapIpV4::Static { ipv4, sub, gateway } => {
                                let gateway = ip_addr_from_str(&gateway);
                                let mut routes = Routes::new(BTreeMap::new());
                                routes.add_default_ipv4_route(gateway).unwrap();
                                let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                                iface.ip_addrs(ip_addrs).routes(routes)
                            }
                        };

                        (iface.finalize(), fd, None)
                    }
                };

                (
                    fd,
                    StcpBackendInterface::Tap {
                        interface: iface,
                        destroy: destroy,
                    },
                )
            }
            #[cfg(feature = "netmap")]
            StcpBackend::NetmapConfig(NetmapDevice::Interface {
                interface,
                mac,
                ipv4,
            }) => {
                let (iface, fd) = {
                    let (interface, parent) = match interface {
                        NetmapInterface::DiscoverFromRoute => {
                            let i = get_default_interface();
                            (i.clone(), i)
                        }
                        NetmapInterface::Interface {
                            netmap_name,
                            parent,
                        } => (netmap_name, parent),
                    };
                    let macaddr = match mac {
                        Mac::Passthru => get_macaddr(&parent),
                        Mac::Static(mac_str) => mac_str,
                        Mac::Random => gen_random_mac(),
                    };
                    let ethernet_addr = mac_str_to_eth_addr(&macaddr);

                    let device =
                        Netmap::new(&interface, &parent, waiting_poll, reduce_mtu_by).unwrap();
                    let fd = device.as_raw_fd();

                    let mut iface = EthernetInterfaceBuilder::new(device)
                        .ethernet_addr(ethernet_addr)
                        .neighbor_cache(neighbor_cache);

                    let iface = match ipv4 {
                        IpV4::Dhcp => panic!("not implemented yet in smoltcp"),
                        IpV4::Passthru => {
                            let mut routes = Routes::new(BTreeMap::new());
                            if let Some(gw) = get_gateway(&parent) {
                                let gateway = ip_addr_from_str(&gw);
                                routes.add_default_ipv4_route(gateway).unwrap();
                            }
                            let (ipv4, sub) = get_ipv4(&parent);
                            let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                            iface.ip_addrs(ip_addrs).routes(routes)
                        }
                        IpV4::Static { ipv4, sub, gateway } => {
                            let gateway = ip_addr_from_str(&gateway);
                            let mut routes = Routes::new(BTreeMap::new());
                            routes.add_default_ipv4_route(gateway).unwrap();
                            let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                            iface.ip_addrs(ip_addrs).routes(routes)
                        }
                    };

                    (iface.finalize(), fd)
                };

                (fd, StcpBackendInterface::Netmap { interface: iface })
            }
            StcpBackend::UsnetConfig(UsnetDevice::Interface {
                interface,
                ipc: UsnetType::UnixDomainSocket,
                mac,
                ipv4,
            }) => {
                let (iface, fd, control_uds) = {
                    let parent = match interface {
                        SystemInterface::DiscoverFromRoute => get_default_interface(),
                        SystemInterface::Interface(parent) => parent,
                    };
                    let macaddr = match mac {
                        Mac::Passthru => get_macaddr(&parent),
                        Mac::Static(mac_str) => mac_str,
                        Mac::Random => gen_random_mac(),
                    };
                    let ethernet_addr = mac_str_to_eth_addr(&macaddr);

                    let tmpsckt = format!(
                        "/run/user/{}/usnet-client-{}-{}",
                        getuid(),
                        process::id(),
                        rand::random::<u64>()
                    );
                    let _ = remove_file(&tmpsckt);
                    let mut control_uds =
                        UnixDatagram::bind(tmpsckt).expect("binding tmp socket failed");
                    let payl = serde_json::to_string(&ClientMessage::RequestUDS(
                        parent.to_owned(),
                        libc::pid_t::from(gettid()) as u64,
                    ))
                    .unwrap();
                    let sent_bytes = control_uds
                        .send_to(payl.as_bytes(), SOCKET_PATH)
                        .expect("cannot send to service unix domain socket");
                    assert_eq!(sent_bytes, payl.len());
                    // receive response
                    let mut buf = [0u8; 1];
                    let iov = [IoVec::from_mut_slice(&mut buf[..])];
                    let mut cmsgspace: CmsgSpace<[RawFd; 1]> = CmsgSpace::new();
                    let msg = recvmsg(
                        control_uds.as_raw_fd(),
                        &iov,
                        Some(&mut cmsgspace),
                        MsgFlags::empty(),
                    )
                    .unwrap();
                    let mut received_fd: Option<RawFd> = None;
                    for cmsg in msg.cmsgs() {
                        if let ControlMessage::ScmRights(fd) = cmsg {
                            assert_eq!(received_fd, None);
                            assert_eq!(fd.len(), 1);
                            received_fd = Some(fd[0]);
                        } else {
                            panic!("unexpected cmsg");
                        }
                    }
                    assert_eq!(
                        match msg.address {
                            Some(SockAddr::Unix(ua)) => {
                                ua.path().map(|p| p.to_str().map(|i| i == SOCKET_PATH))
                            }
                            _ => None,
                        },
                        Some(Some(true))
                    );
                    assert!(!msg
                        .flags
                        .intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC));
                    assert!(received_fd.is_some());

                    let fd = received_fd.unwrap();
                    let device = UnixDomainSocket::new_from_unix_datagram(
                        unsafe { UnixDatagram::from_raw_fd(fd) },
                        &parent,
                        reduce_mtu_by,
                    )
                    .unwrap();

                    let mut iface = EthernetInterfaceBuilder::new(device)
                        .ethernet_addr(ethernet_addr)
                        .neighbor_cache(neighbor_cache);

                    let iface = match ipv4 {
                        IpV4::Dhcp => panic!("not implemented yet in smoltcp"),
                        IpV4::Passthru => {
                            let mut routes = Routes::new(BTreeMap::new());
                            if let Some(gw) = get_gateway(&parent) {
                                let gateway = ip_addr_from_str(&gw);
                                routes.add_default_ipv4_route(gateway).unwrap();
                            }
                            let (ipv4, sub) = get_ipv4(&parent);
                            let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                            iface.ip_addrs(ip_addrs).routes(routes)
                        }
                        IpV4::Static { ipv4, sub, gateway } => {
                            let gateway = ip_addr_from_str(&gateway);
                            let mut routes = Routes::new(BTreeMap::new());
                            routes.add_default_ipv4_route(gateway).unwrap();
                            let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                            iface.ip_addrs(ip_addrs).routes(routes)
                        }
                    };

                    (iface.finalize(), fd, control_uds)
                };

                (
                    fd,
                    StcpBackendInterface::UsnetUds {
                        interface: iface,
                        control: control_uds,
                    },
                )
            }
            #[cfg(feature = "netmap")]
            StcpBackend::UsnetConfig(UsnetDevice::Interface {
                interface,
                ipc: UsnetType::NetmapPipe,
                mac,
                ipv4,
            }) => {
                let (iface, fd, control_uds) = {
                    let parent = match interface {
                        SystemInterface::DiscoverFromRoute => get_default_interface(),
                        SystemInterface::Interface(parent) => parent,
                    };
                    let macaddr = match mac {
                        Mac::Passthru => get_macaddr(&parent),
                        Mac::Static(mac_str) => mac_str,
                        Mac::Random => gen_random_mac(),
                    };
                    let ethernet_addr = mac_str_to_eth_addr(&macaddr);

                    let tmpsckt = format!(
                        "/run/user/{}/usnet-client-{}-{}",
                        getuid(),
                        process::id(),
                        rand::random::<u64>()
                    );
                    let _ = remove_file(&tmpsckt);
                    let mut control_uds =
                        UnixDatagram::bind(tmpsckt).expect("binding tmp socket failed");
                    let payl = serde_json::to_string(&ClientMessage::RequestNetmapPipe(
                        parent.to_owned(),
                        libc::pid_t::from(gettid()) as u64,
                    ))
                    .unwrap();
                    let sent_bytes = control_uds
                        .send_to(payl.as_bytes(), SOCKET_PATH)
                        .expect("cannot send to service unix domain socket");
                    assert_eq!(sent_bytes, payl.len());
                    // receive response
                    let mut buf = [0u8; mem::size_of::<nmreq>()];
                    let iov = [IoVec::from_mut_slice(&mut buf[..])];
                    let mut cmsgspace: CmsgSpace<[RawFd; 1]> = CmsgSpace::new();
                    let msg = recvmsg(
                        control_uds.as_raw_fd(),
                        &iov,
                        Some(&mut cmsgspace),
                        MsgFlags::empty(),
                    )
                    .unwrap();
                    let req: nmreq = unsafe {
                        mem::transmute_copy(&(*(iov[0].as_slice().as_ptr() as *const nmreq)))
                    };
                    let mut received_fd: Option<RawFd> = None;
                    for cmsg in msg.cmsgs() {
                        if let ControlMessage::ScmRights(fd) = cmsg {
                            assert_eq!(received_fd, None);
                            assert_eq!(fd.len(), 1);
                            received_fd = Some(fd[0]);
                        } else {
                            panic!("unexpected cmsg");
                        }
                    }
                    assert_eq!(
                        match msg.address {
                            Some(SockAddr::Unix(ua)) => {
                                ua.path().map(|p| p.to_str().map(|i| i == SOCKET_PATH))
                            }
                            _ => None,
                        },
                        Some(Some(true))
                    );
                    assert!(!msg
                        .flags
                        .intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC));
                    assert!(received_fd.is_some());

                    let fd = received_fd.unwrap();
                    let device =
                        Netmap::new_from_shared_fd(fd, req, &parent, waiting_poll, reduce_mtu_by)
                            .expect("Netmap mmap failed");

                    let mut iface = EthernetInterfaceBuilder::new(device)
                        .ethernet_addr(ethernet_addr)
                        .neighbor_cache(neighbor_cache);

                    let iface = match ipv4 {
                        IpV4::Dhcp => panic!("not implemented yet in smoltcp"),
                        IpV4::Passthru => {
                            let mut routes = Routes::new(BTreeMap::new());
                            if let Some(gw) = get_gateway(&parent) {
                                let gateway = ip_addr_from_str(&gw);
                                routes.add_default_ipv4_route(gateway).unwrap();
                            }
                            let (ipv4, sub) = get_ipv4(&parent);
                            let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                            iface.ip_addrs(ip_addrs).routes(routes)
                        }
                        IpV4::Static { ipv4, sub, gateway } => {
                            let gateway = ip_addr_from_str(&gateway);
                            let mut routes = Routes::new(BTreeMap::new());
                            routes.add_default_ipv4_route(gateway).unwrap();
                            let ip_addrs = [ip_cidr_from_str(&ipv4, sub)];
                            iface.ip_addrs(ip_addrs).routes(routes)
                        }
                    };

                    (iface.finalize(), fd, control_uds)
                };

                (
                    fd,
                    StcpBackendInterface::UsnetNetmap {
                        interface: iface,
                        control: control_uds,
                    },
                )
            }
        }
    }
}

// can be used for avoiding connect port clashes for same endpoint or debugging and visualizing usnetd rules

pub fn is_free_listening_port_for(
    control: &mut UnixDatagram,
    port: u16,
    ip: ClientMessageIp,
) -> bool {
    let payl = serde_json::to_string(&ClientMessage::QueryUsedPorts).unwrap();
    let sent_bytes = control
        .send_to(payl.as_bytes(), SOCKET_PATH)
        .expect("cannot send to service unix domain socket");
    assert_eq!(sent_bytes, payl.len());
    let mut buf = vec![0; 1000000];
    if let Ok((length, sockad)) = control.recv_from(&mut buf) {
        assert_eq!(
            sockad.as_pathname().map(|p| p.to_str()),
            Some(Some(SOCKET_PATH))
        );
        if let Ok(buf_str) = str::from_utf8(&buf[..length]) {
            if let Ok(ClientMessage::QueryUsedPortsAnswer {
                listening,
                connected: _,
            }) = serde_json::from_str(buf_str)
            {
                let triple = (u8::from(IpProtocol::Tcp), ip, port);
                !listening.contains(&triple)
            } else {
                panic!("cannot parse port list answer: {}", buf_str);
            }
        } else {
            panic!("cannot parse port list answer string");
        }
    } else {
        panic!("recv fail");
    }
}

pub fn find_free_connecting_or_listening_port_for(
    connecting: bool,
    control: &mut UnixDatagram,
    ip: ClientMessageIp,
) -> u16 {
    let mut triple = (u8::from(IpProtocol::Tcp), ip, 0);
    let payl = serde_json::to_string(&ClientMessage::QueryUsedPorts).unwrap();
    let sent_bytes = control
        .send_to(payl.as_bytes(), SOCKET_PATH)
        .expect("cannot send to service unix domain socket");
    assert_eq!(sent_bytes, payl.len());
    let mut buf = vec![0; 1000000];
    if let Ok((length, sockad)) = control.recv_from(&mut buf) {
        assert_eq!(
            sockad.as_pathname().map(|p| p.to_str()),
            Some(Some(SOCKET_PATH))
        );
        if let Ok(buf_str) = str::from_utf8(&buf[..length]) {
            if let Ok(ClientMessage::QueryUsedPortsAnswer {
                ref listening,
                ref connected,
            }) = serde_json::from_str(buf_str)
            {
                while {
                    let (_, _, port) = triple;
                    port
                } == 0
                    || if connecting { connected } else { listening }.contains(&triple)
                {
                    let &mut (_, _, ref mut p) = &mut triple;
                    *p = rand::random::<u16>();
                }
            } else {
                panic!("cannot parse port list answer: {}", buf_str);
            }
        } else {
            panic!("cannot parse port list answer string");
        }
    } else {
        panic!("recv fail");
    }
    let (_, _, port) = triple;
    port
}
