//! Host-created loopback sockets for processes behind the local-agent wall.
//!
//! Protocol messages have fixed sizes, contain no paths or hostnames, and use
//! network byte order. Descriptor transfer uses SCM_RIGHTS on an authenticated
//! private Unix connection. A lease identifies an allocation on that connection;
//! it is never an authentication credential.

use std::io::{self, ErrorKind};

#[cfg(target_os = "macos")]
pub mod identity;

#[cfg(target_os = "macos")]
pub mod authority;

#[cfg(target_os = "macos")]
pub mod pool;

#[cfg(target_os = "macos")]
pub mod service;

#[cfg(target_os = "macos")]
pub mod launch;

pub const REQUEST_SIZE: usize = 40;
pub const REPLY_SIZE: usize = 32;
pub const MAX_BACKLOG: u16 = 4096;
const MAGIC: &[u8; 4] = b"SMSK";
const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IpVersion {
    V4,
    V6,
}

impl IpVersion {
    fn code(self) -> u8 {
        match self {
            Self::V4 => 1,
            Self::V6 => 2,
        }
    }

    fn decode(code: u8) -> io::Result<Self> {
        match code {
            1 => Ok(Self::V4),
            2 => Ok(Self::V6),
            _ => invalid("unsupported IP family"),
        }
    }

    fn address(self) -> [u8; 16] {
        let mut address = [0; 16];
        match self {
            Self::V4 => address[..4].copy_from_slice(&[127, 0, 0, 1]),
            Self::V6 => address[15] = 1,
        }
        address
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Bind = 1,
    Listen = 2,
    Connect = 3,
    Release = 4,
    Retain = 5,
}

impl Operation {
    fn decode(code: u8) -> io::Result<Self> {
        match code {
            1 => Ok(Self::Bind),
            2 => Ok(Self::Listen),
            3 => Ok(Self::Connect),
            4 => Ok(Self::Release),
            5 => Ok(Self::Retain),
            _ => invalid("unsupported socket operation"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    Bind {
        ip: IpVersion,
        port: u16,
        reuse_address: bool,
    },
    Listen {
        lease: u64,
        backlog: u16,
    },
    Connect {
        ip: IpVersion,
        port: u16,
    },
    Release {
        lease: u64,
    },
    Retain {
        ip: IpVersion,
        port: u16,
    },
}

impl Request {
    pub fn operation(self) -> Operation {
        match self {
            Self::Bind { .. } => Operation::Bind,
            Self::Listen { .. } => Operation::Listen,
            Self::Connect { .. } => Operation::Connect,
            Self::Release { .. } => Operation::Release,
            Self::Retain { .. } => Operation::Retain,
        }
    }

    fn validate(self) -> io::Result<()> {
        match self {
            Self::Listen { lease, backlog } if lease == 0 || backlog > MAX_BACKLOG => {
                invalid("invalid listen lease or backlog")
            }
            Self::Release { lease: 0 } => invalid("invalid release lease"),
            Self::Connect { port: 0, .. } | Self::Retain { port: 0, .. } => {
                invalid("request requires an allocated test port")
            }
            _ => Ok(()),
        }
    }

    /// Layout: magic/version/op/family/flags (8), port/backlog (4),
    /// reserved (4), lease (8), exact loopback address (16).
    pub fn encode(self) -> io::Result<[u8; REQUEST_SIZE]> {
        self.validate()?;
        let mut frame = [0; REQUEST_SIZE];
        frame[..4].copy_from_slice(MAGIC);
        frame[4] = VERSION;
        frame[5] = self.operation() as u8;
        match self {
            Self::Bind {
                ip,
                port,
                reuse_address,
            } => {
                frame[6] = ip.code();
                frame[7] = u8::from(reuse_address);
                frame[8..10].copy_from_slice(&port.to_be_bytes());
                frame[24..40].copy_from_slice(&ip.address());
            }
            Self::Connect { ip, port } | Self::Retain { ip, port } => {
                frame[6] = ip.code();
                frame[8..10].copy_from_slice(&port.to_be_bytes());
                frame[24..40].copy_from_slice(&ip.address());
            }
            Self::Listen { lease, backlog } => {
                frame[10..12].copy_from_slice(&backlog.to_be_bytes());
                frame[16..24].copy_from_slice(&lease.to_be_bytes());
            }
            Self::Release { lease } => frame[16..24].copy_from_slice(&lease.to_be_bytes()),
        }
        Ok(frame)
    }

    pub fn decode(frame: &[u8]) -> io::Result<Self> {
        if frame.len() != REQUEST_SIZE || &frame[..4] != MAGIC || frame[4] != VERSION {
            return invalid("invalid socket request framing");
        }
        if frame[12..16] != [0; 4] {
            return invalid("nonzero reserved request bytes");
        }
        let op = Operation::decode(frame[5])?;
        let port = u16::from_be_bytes(frame[8..10].try_into().unwrap());
        let backlog = u16::from_be_bytes(frame[10..12].try_into().unwrap());
        let lease = u64::from_be_bytes(frame[16..24].try_into().unwrap());
        let request = match op {
            Operation::Bind | Operation::Connect | Operation::Retain => {
                let ip = IpVersion::decode(frame[6])?;
                if frame[24..40] != ip.address() || backlog != 0 || lease != 0 {
                    return invalid("socket request is not an exact loopback allocation");
                }
                if frame[7] > 1 || (op != Operation::Bind && frame[7] != 0) {
                    return invalid("unsupported socket flags");
                }
                if op == Operation::Bind {
                    Self::Bind {
                        ip,
                        port,
                        reuse_address: frame[7] == 1,
                    }
                } else if op == Operation::Retain {
                    Self::Retain { ip, port }
                } else {
                    Self::Connect { ip, port }
                }
            }
            Operation::Listen | Operation::Release => {
                if frame[6] != 0 || frame[7] != 0 || port != 0 || frame[24..40] != [0; 16] {
                    return invalid("lease request contains unrelated socket parameters");
                }
                if op == Operation::Listen {
                    Self::Listen { lease, backlog }
                } else {
                    if backlog != 0 {
                        return invalid("release request contains a backlog");
                    }
                    Self::Release { lease }
                }
            }
        };
        request.validate()?;
        Ok(request)
    }
}

/// The number of transferred descriptors is declared and checked separately
/// against the actual SCM_RIGHTS message before any descriptor is installed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reply {
    pub operation: Operation,
    pub errno: i32,
    pub ip: Option<IpVersion>,
    pub port: u16,
    pub lease: u64,
    pub descriptors: u8,
}

impl Reply {
    pub fn error(operation: Operation, errno: i32) -> io::Result<Self> {
        if errno <= 0 {
            return invalid("error reply requires a positive errno");
        }
        Ok(Self {
            operation,
            errno,
            ip: None,
            port: 0,
            lease: 0,
            descriptors: 0,
        })
    }

    fn validate(self) -> io::Result<()> {
        if self.errno < 0 {
            return invalid("negative reply errno");
        }
        if self.errno != 0 {
            return if self.descriptors == 0
                && self.ip.is_none()
                && self.port == 0
                && self.lease == 0
            {
                Ok(())
            } else {
                invalid("failed request carries a socket capability")
            };
        }
        let valid = match self.operation {
            Operation::Bind | Operation::Retain => {
                self.descriptors == 1 && self.ip.is_some() && self.port != 0 && self.lease != 0
            }
            Operation::Connect => {
                self.descriptors == 1 && self.ip.is_some() && self.port != 0 && self.lease == 0
            }
            Operation::Listen | Operation::Release => {
                self.descriptors == 0 && self.ip.is_none() && self.port == 0 && self.lease != 0
            }
        };
        if valid {
            Ok(())
        } else {
            invalid("reply capability does not match the socket operation")
        }
    }

    /// Layout: magic/version/op/family/fd-count (8), errno (4), port (2),
    /// reserved (2), lease (8), reserved (8).
    pub fn encode(self) -> io::Result<[u8; REPLY_SIZE]> {
        self.validate()?;
        let mut frame = [0; REPLY_SIZE];
        frame[..4].copy_from_slice(MAGIC);
        frame[4] = VERSION;
        frame[5] = self.operation as u8;
        frame[6] = self.ip.map_or(0, IpVersion::code);
        frame[7] = self.descriptors;
        frame[8..12].copy_from_slice(&self.errno.to_be_bytes());
        frame[12..14].copy_from_slice(&self.port.to_be_bytes());
        frame[16..24].copy_from_slice(&self.lease.to_be_bytes());
        Ok(frame)
    }

    pub fn decode(frame: &[u8]) -> io::Result<Self> {
        if frame.len() != REPLY_SIZE || &frame[..4] != MAGIC || frame[4] != VERSION {
            return invalid("invalid socket reply framing");
        }
        if frame[14..16] != [0; 2] || frame[24..32] != [0; 8] {
            return invalid("nonzero reserved reply bytes");
        }
        let reply = Self {
            operation: Operation::decode(frame[5])?,
            errno: i32::from_be_bytes(frame[8..12].try_into().unwrap()),
            ip: if frame[6] == 0 {
                None
            } else {
                Some(IpVersion::decode(frame[6])?)
            },
            descriptors: frame[7],
            port: u16::from_be_bytes(frame[12..14].try_into().unwrap()),
            lease: u64::from_be_bytes(frame[16..24].try_into().unwrap()),
        };
        reply.validate()?;
        Ok(reply)
    }
}

fn invalid<T>(reason: &'static str) -> io::Result<T> {
    Err(io::Error::new(ErrorKind::InvalidData, reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_request_cannot_change_the_loopback_destination() {
        let request = Request::Bind {
            ip: IpVersion::V4,
            port: 0,
            reuse_address: true,
        };
        let frame = request.encode().unwrap();
        assert_eq!(Request::decode(&frame).unwrap(), request);
        for address in [
            [0; 16],
            [192, 168, 1, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 255, 255, 127, 0, 0, 1],
        ] {
            let mut forged = frame;
            forged[24..40].copy_from_slice(&address);
            assert!(Request::decode(&forged).is_err());
        }
        let ipv6 = Request::Connect {
            ip: IpVersion::V6,
            port: 32123,
        };
        assert_eq!(Request::decode(&ipv6.encode().unwrap()).unwrap(), ipv6);
        let mut mapped = ipv6.encode().unwrap();
        mapped[34..36].copy_from_slice(&[255, 255]);
        assert!(Request::decode(&mapped).is_err());
    }

    #[test]
    fn malformed_frames_and_unrelated_parameters_are_rejected() {
        let frame = Request::Listen {
            lease: 17,
            backlog: 128,
        }
        .encode()
        .unwrap();
        for size in 0..REQUEST_SIZE {
            assert!(Request::decode(&frame[..size]).is_err());
        }
        for (offset, value) in [
            (0, 0),
            (4, 2),
            (5, 99),
            (6, 1),
            (7, 1),
            (8, 1),
            (12, 1),
            (24, 1),
        ] {
            let mut forged = frame;
            forged[offset] = value;
            assert!(Request::decode(&forged).is_err());
        }
        assert!(Request::Listen {
            lease: 0,
            backlog: 128,
        }
        .encode()
        .is_err());
        assert!(Request::Listen {
            lease: 17,
            backlog: MAX_BACKLOG + 1,
        }
        .encode()
        .is_err());
    }

    #[test]
    fn failed_reply_cannot_smuggle_a_descriptor_or_lease() {
        let reply = Reply::error(Operation::Bind, libc::EACCES).unwrap();
        let frame = reply.encode().unwrap();
        assert_eq!(Reply::decode(&frame).unwrap(), reply);
        for (offset, value) in [(6, 1), (7, 1), (12, 1), (16, 1), (24, 1)] {
            let mut forged = frame;
            forged[offset] = value;
            assert!(Reply::decode(&forged).is_err());
        }
        let success = Reply {
            operation: Operation::Bind,
            errno: 0,
            ip: Some(IpVersion::V6),
            port: 32123,
            lease: 17,
            descriptors: 1,
        };
        assert_eq!(Reply::decode(&success.encode().unwrap()).unwrap(), success);
        let mut missing_fd = success.encode().unwrap();
        missing_fd[7] = 0;
        assert!(Reply::decode(&missing_fd).is_err());
    }
}
