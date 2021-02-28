use smoltcp::time::{Duration, Instant};
use std::fmt;

use std::fs::remove_file;

extern crate usnet_devices;
#[cfg(feature = "netmap")]
use self::usnet_devices::Netmap;
use self::usnet_devices::{RawSocket, TapInterface, UnixDomainSocket};

use smoltcp;
use smoltcp::iface::EthernetInterface;
use smoltcp::socket::SocketSet;
use smoltcp::wire::{EthernetAddress, IpProtocol};

use serde_json;
use std::io;
use std::net::IpAddr;
use std::os::unix::net::UnixDatagram;
use std::process::Command;

extern crate libusnetd;
use self::libusnetd::{ClientMessage, ClientMessageIp, WantMsg, SOCKET_PATH};

pub enum StcpBackendInterface {
    Raw(EthernetInterface<'static, RawSocket>),
    Tap {
        interface: EthernetInterface<'static, TapInterface>,
        destroy: Option<String>,
    },
    MacVtap {
        interface: EthernetInterface<'static, TapInterface>,
        destroy: Option<String>,
    },
    #[cfg(feature = "netmap")]
    Netmap {
        interface: EthernetInterface<'static, Netmap>,
    },
    UsnetUds {
        interface: EthernetInterface<'static, UnixDomainSocket>,
        control: UnixDatagram,
    },
    #[cfg(feature = "netmap")]
    UsnetNetmap {
        interface: EthernetInterface<'static, Netmap>,
        control: UnixDatagram,
    },
}

impl fmt::Debug for StcpBackendInterface {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self)
    }
}

impl fmt::Display for StcpBackendInterface {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let typedesc = match self {
            StcpBackendInterface::Raw(_) => "direct raw socket",
            StcpBackendInterface::MacVtap {
                interface: _,
                destroy: _,
            } => "direct macvtap",
            StcpBackendInterface::Tap {
                interface: _,
                destroy: _,
            } => "direct tap",
            #[cfg(feature = "netmap")]
            StcpBackendInterface::Netmap { interface: _ } => "direct netmap",
            #[cfg(feature = "netmap")]
            StcpBackendInterface::UsnetNetmap {
                interface: _,
                control: _,
            } => "usnetd netmap",
            StcpBackendInterface::UsnetUds {
                interface: _,
                control: _,
            } => "usnetd unix domain socket",
        };
        let ips: Vec<String> = self.ips().iter().map(|c| format!("{}", c)).collect();
        write!(
            f,
            "{} {} {}",
            typedesc,
            ips[..].join(", "),
            self.ethernet_addr()
        )
    }
}

impl StcpBackendInterface {
    pub fn add_port_match(
        &mut self,
        ipa: IpAddr,
        port: Option<u16>,
        remote_addr: Option<IpAddr>,
        remote_port: Option<u16>,
        protocol: IpProtocol,
    ) -> io::Result<()> {
        let ipv4 = self.ips()[0].address();
        match self.control() {
            Some(control) => {
                let ipstr = if ipa.is_unspecified() {
                    format!("{}", ipv4)
                } else {
                    format!("{}", ipa)
                };
                let remote_ip = remote_addr.map(|i| ClientMessageIp::Ipv4(format!("{}", i)));
                let want = WantMsg {
                    dst_addr: ClientMessageIp::Ipv4(ipstr),
                    dst_port: port,
                    src_addr: remote_ip,
                    src_port: remote_port,
                    protocol: u8::from(protocol),
                };
                let payl = serde_json::to_string(&ClientMessage::AddMatch(want)).unwrap();
                let sent_bytes = control
                    .send_to(payl.as_bytes(), SOCKET_PATH)
                    .expect("cannot send to service unix domain socket");
                assert_eq!(sent_bytes, payl.len());
                let mut succ = vec![0; 2];
                if let (2, ua) = control.recv_from(&mut succ)? {
                    assert_eq!(
                        ua.as_pathname()
                            .map(|p| p.to_str().map(|i| i == SOCKET_PATH)),
                        Some(Some(true))
                    );
                    if succ != "OK".as_bytes() {
                        Err(io::Error::new(io::ErrorKind::Other, "port not free"))
                    } else {
                        Ok(())
                    }
                } else {
                    panic!("wrong answer");
                }
            }
            _ => Ok(()),
        }
    }
    pub fn control(&mut self) -> Option<&mut UnixDatagram> {
        match self {
            #[cfg(feature = "netmap")]
            StcpBackendInterface::UsnetNetmap {
                interface: _,
                ref mut control,
            } => Some(control),
            StcpBackendInterface::UsnetUds {
                interface: _,
                ref mut control,
            } => Some(control),
            _ => None,
        }
    }
    pub fn ethernet_addr(&self) -> EthernetAddress {
        match self {
            StcpBackendInterface::Raw(ref iface) => iface.ethernet_addr(),
            StcpBackendInterface::MacVtap {
                interface: ref iface,
                destroy: _,
            }
            | StcpBackendInterface::Tap {
                interface: ref iface,
                destroy: _,
            } => iface.ethernet_addr(),
            #[cfg(feature = "netmap")]
            StcpBackendInterface::Netmap {
                interface: ref iface,
            } => iface.ethernet_addr(),
            #[cfg(feature = "netmap")]
            StcpBackendInterface::UsnetNetmap {
                interface: ref iface,
                control: _,
            } => iface.ethernet_addr(),
            StcpBackendInterface::UsnetUds {
                interface: ref iface,
                control: _,
            } => iface.ethernet_addr(),
        }
    }
    pub fn ips(&self) -> &[smoltcp::wire::IpCidr] {
        match self {
            StcpBackendInterface::Raw(ref iface) => iface.ip_addrs(),
            StcpBackendInterface::MacVtap {
                interface: ref iface,
                destroy: _,
            }
            | StcpBackendInterface::Tap {
                interface: ref iface,
                destroy: _,
            } => iface.ip_addrs(),
            #[cfg(feature = "netmap")]
            StcpBackendInterface::Netmap {
                interface: ref iface,
            } => iface.ip_addrs(),
            #[cfg(feature = "netmap")]
            StcpBackendInterface::UsnetNetmap {
                interface: ref iface,
                control: _,
            } => iface.ip_addrs(),
            StcpBackendInterface::UsnetUds {
                interface: ref iface,
                control: _,
            } => iface.ip_addrs(),
        }
    }
    pub fn poll(&mut self, sockets: &mut SocketSet, timestamp: Instant) -> smoltcp::Result<bool> {
        match self {
            StcpBackendInterface::Raw(ref mut iface) => iface.poll(sockets, timestamp),
            StcpBackendInterface::MacVtap {
                interface: ref mut iface,
                destroy: _,
            }
            | StcpBackendInterface::Tap {
                interface: ref mut iface,
                destroy: _,
            } => iface.poll(sockets, timestamp),
            #[cfg(feature = "netmap")]
            StcpBackendInterface::Netmap {
                interface: ref mut iface,
            } => iface.poll(sockets, timestamp),
            #[cfg(feature = "netmap")]
            StcpBackendInterface::UsnetNetmap {
                interface: ref mut iface,
                control: _,
            } => iface.poll(sockets, timestamp),
            StcpBackendInterface::UsnetUds {
                interface: ref mut iface,
                control: _,
            } => iface.poll(sockets, timestamp),
        }
    }
    pub fn poll_delay(&self, sockets: &SocketSet, timestamp: Instant) -> Option<Duration> {
        match self {
            StcpBackendInterface::Raw(ref iface) => iface.poll_delay(sockets, timestamp),
            StcpBackendInterface::MacVtap {
                interface: ref iface,
                destroy: _,
            }
            | StcpBackendInterface::Tap {
                interface: ref iface,
                destroy: _,
            } => iface.poll_delay(sockets, timestamp),
            #[cfg(feature = "netmap")]
            StcpBackendInterface::Netmap {
                interface: ref iface,
            } => iface.poll_delay(sockets, timestamp),
            #[cfg(feature = "netmap")]
            StcpBackendInterface::UsnetNetmap {
                interface: ref iface,
                control: _,
            } => iface.poll_delay(sockets, timestamp),
            StcpBackendInterface::UsnetUds {
                interface: ref iface,
                control: _,
            } => iface.poll_delay(sockets, timestamp),
        }
    }
}

impl Drop for StcpBackendInterface {
    fn drop(&mut self) {
        match self {
            StcpBackendInterface::MacVtap {
                interface: _,
                destroy: Some(ref name),
            } => {
                let _ = Command::new("pkexec")
                    .args(&["ip", "link", "delete", name])
                    .status();
            }
            StcpBackendInterface::Tap {
                interface: _,
                destroy: Some(ref name),
            } => {
                let _ = Command::new("pkexec")
                    .args(&["ip", "link", "delete", name])
                    .status();
            }
            StcpBackendInterface::UsnetUds {
                interface: _,
                ref mut control,
            } => delete(control),
            #[cfg(feature = "netmap")]
            StcpBackendInterface::UsnetNetmap {
                interface: _,
                ref mut control,
            } => delete(control),
            _ => {}
        }
    }
}

fn delete(control: &mut UnixDatagram) {
    // unregister from usnetd
    let payl = serde_json::to_string(&ClientMessage::DeleteClient).unwrap();
    let sent_bytes = control
        .send_to(payl.as_bytes(), SOCKET_PATH)
        .expect("cannot send to service unix domain socket");
    assert_eq!(sent_bytes, payl.len());
    if let Ok(sockaddr) = control.local_addr() {
        if let Some(path) = sockaddr.as_pathname() {
            match remove_file(path) {
                Err(err) => {
                    error!("drop: could not remove {}: {}", path.display(), err);
                }
                _ => {}
            }
        } else {
            error!("drop: no pathname");
        }
    } else {
        error!("drop: no local addr");
    }
}
