use rand;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, Ipv4Address};
use std::env;
use std::fs;
use std::io::prelude::*;
use std::process::Command;
use std::str::{self, FromStr};

// util functions for Linux

pub fn get_default_interface() -> String {
    let coutput = Command::new("ip")
        .args(&["route", "get", "1"])
        .output()
        .expect("command ip route get 1 failed");
    let sout1 = String::from_utf8(coutput.stdout).unwrap();
    let sout = sout1.split(' ').collect::<Vec<_>>();
    let origifname = sout[4].to_string();
    origifname.replace("usnetd", "")
}

pub fn get_gateway(interface: &str) -> Option<String> {
    let coutput = Command::new("ip")
        .args(&["-4", "route", "get", "1", "oif", interface])
        .output()
        .expect("command ip route get 1 oif IF failed");
    let sout1 = String::from_utf8(coutput.stdout).unwrap();
    let sout = sout1.split(' ').collect::<Vec<_>>();
    let gateway = sout[2];
    if gateway == interface {
        if interface.ends_with("usnetd") {
            debug!("extracted no gateway for {}", interface);
            None
        } else {
            debug!(
                "extracted no gateway for {}, trying {}usnetd",
                interface, interface
            );
            get_gateway(&(interface.to_string() + "usnetd"))
        }
    } else {
        debug!("extracted gateway {} for {}", gateway, interface);
        Some(gateway.to_string())
    }
}

pub fn ip_addr_from_str(ipv4: &str) -> Ipv4Address {
    Ipv4Address::from_str(ipv4).expect(&format!("ipv4 conv not working for {}", ipv4))
}

pub fn ip_cidr_from_str(ipv4: &str, sub: u8) -> IpCidr {
    IpCidr::new(IpAddress::from_str(ipv4).expect("ip conv"), sub)
}

pub fn get_ipv4(interface: &str) -> (String, u8) {
    let ioutput = Command::new("ip")
        .args(&["-4", "a", "show", "dev", interface])
        .output()
        .expect("command ip show failed");
    let iout1a = String::from_utf8(ioutput.stdout).unwrap();
    let iout1 = iout1a.split("inet ").collect::<Vec<_>>()[1];
    let ipandsub = iout1.split(' ').collect::<Vec<_>>()[0]
        .split('/')
        .collect::<Vec<_>>();
    let ipv4 = ipandsub[0].to_string();
    let sub = u8::from_str(ipandsub[1]).expect("net size not found");
    (ipv4, sub)
}

pub fn get_macaddr(interface: &str) -> String {
    let mut macaddr = String::new();
    {
        let mut f = fs::File::open("/sys/class/net/".to_owned() + interface + "/address")
            .expect("invalid interface");
        f.read_to_string(&mut macaddr)
            .expect("can not read mac addr");
    }
    macaddr
}

pub fn mac_str_to_eth_addr(macaddr: &str) -> EthernetAddress {
    let m = macaddr
        .trim()
        .split(':')
        .map(|s| u8::from_str_radix(s, 16).unwrap())
        .collect::<Vec<_>>();
    EthernetAddress([m[0], m[1], m[2], m[3], m[4], m[5]])
}

pub fn create_macvtap(ifname: &str, origifname: &str, mode: &str, macaddr: &str) {
    let _ = Command::new("pkexec")
        .args(&[
            "ip", "link", "add", "link", origifname, "name", ifname, "type", "macvtap", "mode",
            mode,
        ])
        .status();
    let _ = Command::new("pkexec")
        .args(&["ip", "link", "set", ifname, "address", &macaddr, "up"])
        .status();
    let mut ifindex = String::new();
    {
        let mut f = fs::File::open("/sys/class/net/".to_owned() + ifname + "/ifindex")
            .expect("invalid macvtap interface");
        f.read_to_string(&mut ifindex)
            .expect("can not read macvtap ifindex");
    }
    let username = env::var("USER").unwrap();
    let _ = Command::new("pkexec")
        .args(&[
            "chown",
            &format!("{}:{}", username, username),
            &format!("/dev/tap{}", ifindex.split('\n').next().unwrap()),
        ])
        .status();
}

pub fn get_macvtap_parent(ifname: &str) -> String {
    let ioutput = Command::new("ip")
        .args(&["a", "show", "dev", ifname])
        .output()
        .expect("command ip show failed");
    let iout1a = String::from_utf8(ioutput.stdout).unwrap();
    let iout1 = iout1a.split("@").collect::<Vec<_>>()[1];
    let parent = iout1.split(':').collect::<Vec<_>>()[0].to_string();
    parent
}

pub fn gen_random_mac() -> String {
    let (m0, m1, m2, m3, m4, m5) = rand::random::<(u8, u8, u8, u8, u8, u8)>();
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        m0, m1, m2, m3, m4, m5
    )
}

pub fn create_tap(ifname: &str, host_ip: &str, sub: u8) {
    let username = env::var("USER").unwrap();
    let _ = Command::new("pkexec")
        .args(&[
            "ip", "tuntap", "add", "name", ifname, "mode", "tap", "user", &username,
        ])
        .status();
    let _ = Command::new("pkexec")
        .args(&["ip", "link", "set", ifname, "up"])
        .status();
    let _ = Command::new("pkexec")
        .args(&[
            "ip",
            "addr",
            "add",
            &format!("{}/{}", host_ip, sub),
            "dev",
            ifname,
        ])
        .status();
}

pub fn read_kernel_local_port_range() -> (u16, u16) {
    let mut contents = String::new();
    {
        let mut f = fs::File::open("/proc/sys/net/ipv4/ip_local_port_range")
            .expect("no ipv4 port range proc file found");
        f.read_to_string(&mut contents)
            .expect("can not read port range file");
    }
    let mut splitted = contents.split_whitespace();
    let lower = u16::from_str(splitted.next().unwrap()).expect("could not parse1");
    let upper = u16::from_str(splitted.next().unwrap()).expect("could not parse2");
    (lower, upper)
}
