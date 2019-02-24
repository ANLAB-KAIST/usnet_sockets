//! Domain Name System (DNS) communication protocol.
// In-tree copy of https://github.com/murarth/resolve due to circular dependencies
#![deny(missing_docs)]

pub use self::address::address_name;
pub use self::config::DnsConfig;
pub use self::idna::{to_ascii, to_unicode};
pub use self::message::{DecodeError, EncodeError, Message, Question, Resource, MESSAGE_LIMIT};
pub use self::record::{Class, Record, RecordType};
pub use self::resolver::{resolve_addr, resolve_host, DnsResolver};
pub use self::socket::{DnsSocket, Error};

pub mod address;
pub mod config;
pub mod hostname;
pub mod hosts;
pub mod idna;
pub mod message;
pub mod record;
pub mod resolv_conf;
pub mod resolver;
pub mod socket;
