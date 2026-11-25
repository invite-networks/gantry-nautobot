//! `allocate`: an IPv4 address for a pod interface from Nautobot IPAM.
//!
//! The pool is a Prefix: `prefix:` in the address map (a CIDR or an ID),
//! else the Prefix holding the parent interface's own address, else the
//! Prefix of the cluster's Container Network on the parent's VLAN. A pod
//! that already has an address in that Prefix gets it back; otherwise the
//! first free address that is not the gateway is created.

use crate::Ctx;
use crate::nautobot::{CONTAINERS, Client, id, ref_id};
use crate::sync::{Placement, containing_prefix, find_pod, placement};
use ipnet::{IpNet, Ipv4Net};
use serde_json::{Value, json};
use std::net::Ipv4Addr;

/// How many free addresses to look at before giving up.
const CANDIDATES: usize = 32;

pub fn allocate(c: &Ctx, data: &Value) -> Result<Value, String> {
    let iface = data.get("interface").cloned().unwrap_or(Value::Null);
    let opts = data.get("address").cloned().unwrap_or(Value::Null);
    let pod = data.get("pod").and_then(Value::as_str).ok_or("no pod")?;
    let ifname = iface.get("name").and_then(Value::as_str).unwrap_or("eth0");
    let parent = iface.get("parent").and_then(Value::as_str).unwrap_or("");
    let place = placement(c)?;

    let prefix = pool(c, &place, &opts, parent)?;
    let net = prefix_net(&prefix)?;
    let prefix_id = id(&prefix).ok_or("prefix without an id")?.to_string();

    if let Some(a) = current_address(c, &place, pod, ifname, &net)? {
        crate::log(&c.project, &format!("{pod} {ifname}: keeps {a}"));
        return Ok(json!({ "address": a.to_string() }));
    }

    let gateway: Option<Ipv4Addr> = iface
        .get("gateway")
        .and_then(Value::as_str)
        .and_then(|g| g.parse().ok())
        .or_else(|| first_host(&net));
    let free = c.nb.get(
        &format!("ipam/prefixes/{prefix_id}/available-ips"),
        &[("limit", CANDIDATES.to_string())],
    )?;
    let free = free
        .as_array()
        .cloned()
        .or_else(|| free.get("results").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let description = format!("gantry {}/{pod} {ifname}", c.project);
    let mut last_err = None;
    for cand in free
        .iter()
        .filter_map(|v| v.get("address").and_then(Value::as_str))
    {
        let Ok(IpNet::V4(a)) = cand.parse::<IpNet>() else {
            continue;
        };
        if Some(a.addr()) == gateway || a.addr() == net.network() || a.addr() == net.broadcast() {
            continue;
        }
        let address = Ipv4Net::new(a.addr(), net.prefix_len()).map_err(|e| e.to_string())?;
        // Creating it is the reservation. Someone else taking the same
        // address first fails the create, and the next one is tried.
        match c.nb.post(
            "ipam/ip-addresses",
            &json!({
                "address": address.to_string(),
                "status": {"name": c.ip_status},
                "parent": prefix_id,
                "description": description,
            }),
        ) {
            Ok(_) => {
                crate::log(&c.project, &format!("{pod} {ifname}: allocated {address}"));
                return Ok(json!({ "address": address.to_string() }));
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(match last_err {
        Some(e) => format!("no address could be reserved in {net}: {e}"),
        None => format!("no free address in {net}"),
    })
}

/// The Prefix to allocate from.
fn pool(c: &Ctx, place: &Placement, opts: &Value, parent: &str) -> Result<Value, String> {
    let nb = &c.nb;
    if let Some(p) = opts.get("prefix").and_then(Value::as_str) {
        if p.parse::<IpNet>().is_ok() {
            let mut q = vec![("prefix", p.to_string())];
            if let Some(ns) = opts.get("namespace").and_then(Value::as_str) {
                q.push(("namespace", ns.to_string()));
            }
            return nb
                .one("ipam/prefixes", &q)?
                .ok_or_else(|| format!("no Prefix {p} in Nautobot"));
        }
        return nb.get(&format!("ipam/prefixes/{p}"), &[]);
    }
    if let Some(net) = crate::host::parent_network(parent) {
        if let Some(p) = containing_prefix(nb, net.addr())? {
            return Ok(p);
        }
    }
    if let Some(vid) = crate::host::vlan_id(parent) {
        if let Some(p) = vlan_prefix(nb, place, vid)? {
            return Ok(p);
        }
    }
    Err(format!(
        "no Prefix found for parent {parent:?} (its own address, or a Container Network on its VLAN); set prefix: in the address map"
    ))
}

/// The first Prefix of a Container Network on VLAN `vid`, preferring the
/// node's cluster.
fn vlan_prefix(nb: &Client, place: &Placement, vid: u16) -> Result<Option<Value>, String> {
    let path = format!("{CONTAINERS}/container-networks");
    let vlans = nb.list("ipam/vlans", &[("vid", vid.to_string())])?;
    let mut nets = Vec::new();
    for v in &vlans {
        let Some(vlan) = id(v) else { continue };
        nets.extend(nb.list(&path, &[("vlan", vlan.to_string())])?);
    }
    nets.sort_by_key(|n| ref_id(n, "cluster") != place.cluster);
    for n in nets {
        let prefixes = n
            .get("prefixes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for p in prefixes {
            let pid = match &p {
                Value::String(s) => s.clone(),
                other => match id(other) {
                    Some(s) => s.to_string(),
                    None => continue,
                },
            };
            let prefix = nb.get(&format!("ipam/prefixes/{pid}"), &[])?;
            if prefix_net(&prefix).is_ok() {
                return Ok(Some(prefix));
            }
        }
    }
    Ok(None)
}

/// The address the pod's interface already has in `net`, so a recreated
/// pod keeps it. A `down` pod no longer holds it in Nautobot, and the pod
/// is the same one, so taking it back is safe.
fn current_address(
    c: &Ctx,
    place: &Placement,
    pod: &str,
    ifname: &str,
    net: &Ipv4Net,
) -> Result<Option<Ipv4Net>, String> {
    let Some(rec) = find_pod(c, place, pod)? else {
        return Ok(None);
    };
    let pod_id = id(&rec).ok_or("pod without an id")?;
    let Some(iface) = c.nb.one(
        &format!("{CONTAINERS}/container-interfaces"),
        &[
            ("pod", pod_id.to_string()),
            ("name", ifname.to_string()),
            // Nautobot 3 leaves many-to-many fields out unless asked.
            ("exclude_m2m", "false".into()),
        ],
    )?
    else {
        return Ok(None);
    };
    for ip in iface
        .get("ip_addresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(ip_id) = id(ip)
            .map(String::from)
            .or_else(|| ip.as_str().map(String::from))
        else {
            continue;
        };
        let rec = c.nb.get(&format!("ipam/ip-addresses/{ip_id}"), &[])?;
        let Some(text) = rec.get("address").and_then(Value::as_str) else {
            continue;
        };
        if let Ok(IpNet::V4(a)) = text.parse::<IpNet>()
            && net.contains(&a.addr())
        {
            return Ok(Some(
                Ipv4Net::new(a.addr(), net.prefix_len()).map_err(|e| e.to_string())?,
            ));
        }
    }
    Ok(None)
}

fn prefix_net(prefix: &Value) -> Result<Ipv4Net, String> {
    let text = prefix
        .get("prefix")
        .and_then(Value::as_str)
        .ok_or("prefix without a prefix")?;
    match text.parse::<IpNet>() {
        Ok(IpNet::V4(n)) => Ok(n.trunc()),
        _ => Err(format!("Prefix {text} is not IPv4")),
    }
}

/// The first usable host, gantry's default gateway. A /31 or /32 has none.
fn first_host(net: &Ipv4Net) -> Option<Ipv4Addr> {
    (net.prefix_len() < 31).then(|| Ipv4Addr::from(u32::from(net.network()) + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_and_prefix() {
        let n: Ipv4Net = "192.0.2.216/29".parse().unwrap();
        assert_eq!(first_host(&n), Some("192.0.2.217".parse().unwrap()));
        assert_eq!(first_host(&"192.0.2.0/31".parse().unwrap()), None);
        assert_eq!(prefix_net(&json!({"prefix": "192.0.2.218/29"})).unwrap(), n);
        assert!(prefix_net(&json!({"prefix": "2001:db8::/64"})).is_err());
    }
}
