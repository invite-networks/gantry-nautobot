//! What this host says about a parent interface: its own IPv4 network and
//! its VLAN ID.

use ipnet::{IpNet, Ipv4Net};

/// The IPv4 network of the parent's own address, when it has one.
pub fn parent_network(parent: &str) -> Option<IpNet> {
    let addrs = nix::ifaddrs::getifaddrs().ok()?;
    for a in addrs.filter(|a| a.interface_name == parent) {
        let (Some(addr), Some(mask)) = (a.address, a.netmask) else {
            continue;
        };
        let (Some(ip), Some(m)) = (addr.as_sockaddr_in(), mask.as_sockaddr_in()) else {
            continue;
        };
        let len = u32::from(m.ip()).count_ones() as u8;
        if let Ok(net) = Ipv4Net::new(ip.ip(), len) {
            return Some(IpNet::V4(net));
        }
    }
    None
}

/// The parent's 802.1Q VLAN ID: from the kernel when it is a VLAN
/// interface, else from a name such as `vlan4` or `eth0.4`.
pub fn vlan_id(parent: &str) -> Option<u16> {
    std::fs::read_to_string(format!("/proc/net/vlan/{parent}"))
        .ok()
        .and_then(|t| vid_from_proc(&t))
        .or_else(|| vid_from_name(parent))
}

fn vid_from_proc(text: &str) -> Option<u16> {
    let rest = &text[text.find("VID:")? + 4..];
    rest.split_whitespace().next()?.parse().ok()
}

fn vid_from_name(name: &str) -> Option<u16> {
    let digits = name
        .strip_prefix("vlan")
        .or_else(|| name.rsplit_once('.').map(|(_, v)| v))?;
    let vid: u16 = digits.parse().ok()?;
    (1..=4094).contains(&vid).then_some(vid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vids() {
        let proc = "vlan4  VID: 4\t REORDER_HDR: 1  dev->priv_flags: 1021\n";
        assert_eq!(vid_from_proc(proc), Some(4));
        assert_eq!(vid_from_name("vlan120"), Some(120));
        assert_eq!(vid_from_name("eth0.30"), Some(30));
        assert_eq!(vid_from_name("eth0"), None);
        assert_eq!(vid_from_name("vlan0"), None);
        assert_eq!(vid_from_name("vlanx"), None);
    }
}
