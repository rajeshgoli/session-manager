//! Inspect host interface prefixes for globally addressed LAN/host destinations.
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

#[derive(Clone, Copy)]
pub(super) struct Network {
    pub address: IpAddr,
    pub mask: IpAddr,
}
impl Network {
    pub fn contains(self, address: IpAddr) -> bool {
        let address = match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(address),
            _ => address,
        };
        match (self.address, self.mask, address) {
            (IpAddr::V4(local), IpAddr::V4(mask), IpAddr::V4(remote)) => {
                u32::from(local) & u32::from(mask) == u32::from(remote) & u32::from(mask)
            }
            (IpAddr::V6(local), IpAddr::V6(mask), IpAddr::V6(remote)) => {
                u128::from(local) & u128::from(mask) == u128::from(remote) & u128::from(mask)
            }
            _ => false,
        }
    }
}
pub(super) trait Networks: Send + Sync {
    fn current(&self) -> io::Result<Vec<Network>>;
}
pub(super) struct HostNetworks;
impl Networks for HostNetworks {
    fn current(&self) -> io::Result<Vec<Network>> {
        let mut head = std::ptr::null_mut();
        // SAFETY: getifaddrs initializes head to a linked list owned by this call.
        if unsafe { libc::getifaddrs(&mut head) } != 0 {
            return Err(io::Error::last_os_error());
        }
        struct List(*mut libc::ifaddrs);
        impl Drop for List {
            fn drop(&mut self) {
                unsafe {
                    libc::freeifaddrs(self.0);
                }
            }
        }
        let _list = List(head);
        let mut networks = Vec::new();
        let mut current = head;
        while !current.is_null() {
            // SAFETY: each node belongs to the live getifaddrs list until _list drops.
            let node = unsafe { &*current };
            if let Some(address) = unsafe { ip_address(node.ifa_addr) } {
                // Always exclude the host address even if no mask is available.
                let full_mask = match address {
                    IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::from(u32::MAX)),
                    IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::from(u128::MAX)),
                };
                networks.push(Network {
                    address,
                    mask: full_mask,
                });
                if let Some(mask) = unsafe { ip_address(node.ifa_netmask) } {
                    networks.push(Network { address, mask });
                }
            }
            current = node.ifa_next;
        }
        if networks.is_empty() {
            return Err(io::Error::other("no host interface addresses"));
        }
        Ok(networks)
    }
}
/// Caller supplies only sockaddr pointers obtained from getifaddrs, whose family
/// determines the concrete structure and which remain valid for the list lifetime.
unsafe fn ip_address(address: *const libc::sockaddr) -> Option<IpAddr> {
    if address.is_null() {
        return None;
    }
    match unsafe { (*address).sa_family as i32 } {
        libc::AF_INET => {
            let address = unsafe { &*address.cast::<libc::sockaddr_in>() };
            Some(IpAddr::V4(Ipv4Addr::from(
                address.sin_addr.s_addr.to_ne_bytes(),
            )))
        }
        libc::AF_INET6 => {
            let address = unsafe { &*address.cast::<libc::sockaddr_in6>() };
            Some(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_lan_prefixes_and_host_addresses_are_local() {
        let v4 = Network {
            address: "8.8.8.1".parse().unwrap(),
            mask: "255.255.255.0".parse().unwrap(),
        };
        let v6 = Network {
            address: "2606:4700:1234:5678::1".parse().unwrap(),
            mask: "ffff:ffff:ffff:ffff::".parse().unwrap(),
        };
        assert!(v4.contains("8.8.8.8".parse().unwrap()));
        assert!(v4.contains("::ffff:8.8.8.8".parse().unwrap()));
        assert!(!v4.contains("8.8.4.4".parse().unwrap()));
        assert!(v6.contains("2606:4700:1234:5678::abcd".parse().unwrap()));
        assert!(!v6.contains("2606:4700:1234:5679::abcd".parse().unwrap()));
        let networks = HostNetworks.current().unwrap();
        for network in &networks {
            assert!(networks
                .iter()
                .any(|prefix| prefix.contains(network.address)));
        }
    }
}
