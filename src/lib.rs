#[macro_use]
extern crate lazy_static;

extern crate libc;
extern crate parking_lot;
extern crate smoltcp;

#[macro_use]
extern crate log;

extern crate rand;

#[macro_use]
extern crate serde_derive;

extern crate idna as external_idna;
//extern crate resolve;
pub mod resolve;

extern crate nix;
extern crate serde_json;

pub mod apimultithread;
pub mod apisinglethread;
pub mod device;
pub mod system;
pub mod usnetconfig;

#[cfg(feature = "multi")]
pub use apimultithread::{TcpListener, TcpStream, UdpSocket, UsnetToSocketAddrs};

#[cfg(feature = "single")]
pub use apisinglethread::{TcpListener, TcpStream};

#[cfg(feature = "host")]
pub use std::net::{TcpListener, TcpStream};
