//! Recording what gantry did: `post_up` writes each pod with its
//! interfaces, addresses, containers and volumes; `post_down` marks them
//! `down`, which stamps State Changed so a cleanup job can key off it.

use crate::Ctx;
use crate::nautobot::{CONTAINERS, Client, id, ref_id};
use ipnet::IpNet;
use serde_json::{Value, json};

/// Where this host is in Nautobot.
pub struct Placement {
    /// `{"id", "object_type"}`, as the invite-nautobot `node` field takes it.
    pub node: Value,
    /// The node's Container Cluster, when it is a member of one.
    pub cluster: Option<String>,
}

impl Placement {
    /// Query value for the `cluster` filter.
    pub fn cluster_filter(&self) -> String {
        self.cluster.clone().unwrap_or_else(|| "null".into())
    }
}

/// Finds the Device, else the Virtual Machine, named after this host (or
/// the configured node), and the Container Cluster it belongs to.
pub fn placement(c: &Ctx) -> Result<Placement, String> {
    let mut names = vec![];
    if let Some(n) = &c.node {
        names.push(n.clone());
    } else {
        names.push(c.host.clone());
        if let Some((short, _)) = c.host.split_once('.') {
            names.push(short.to_string());
        }
    }
    for (path, object_type, member_filter) in [
        ("dcim/devices", "dcim.device", "device"),
        (
            "virtualization/virtual-machines",
            "virtualization.virtualmachine",
            "virtual_machine",
        ),
    ] {
        for name in &names {
            let Some(node) = c.nb.one(path, &[("name", name.clone())])? else {
                continue;
            };
            let node_id = id(&node).ok_or("node without an id")?.to_string();
            let cluster =
                c.nb.list(
                    &format!("{CONTAINERS}/cluster-members"),
                    &[(member_filter, node_id.clone())],
                )?
                .first()
                .and_then(|m| ref_id(m, "cluster"));
            return Ok(Placement {
                node: json!({"id": node_id, "object_type": object_type}),
                cluster,
            });
        }
    }
    Err(format!(
        "no Device or Virtual Machine named {} in Nautobot; set node in the plugin config",
        names.join(" or ")
    ))
}

/// The Pod record for a gantry pod, looked up by its scope.
pub fn find_pod(c: &Ctx, place: &Placement, pod: &str) -> Result<Option<Value>, String> {
    c.nb.one(
        &format!("{CONTAINERS}/pods"),
        &[
            ("name", pod.to_string()),
            ("compose_project", c.project.clone()),
            ("cluster", place.cluster_filter()),
        ],
    )
}

pub fn up(c: &Ctx, data: &Value) -> Result<(), String> {
    let place = placement(c)?;
    let mut errors = Vec::new();
    for pod in pods(data) {
        if let Err(e) = up_pod(c, &place, pod) {
            errors.push(format!("pod {}: {e}", str_of(pod, "name")));
        }
    }
    done(errors)
}

pub fn down(c: &Ctx, data: &Value) -> Result<(), String> {
    let place = placement(c)?;
    let mut errors = Vec::new();
    for pod in pods(data) {
        let name = str_of(pod, "name");
        let r = (|| -> Result<(), String> {
            let Some(rec) = find_pod(c, &place, name)? else {
                return Ok(());
            };
            let pod_id = id(&rec).ok_or("pod without an id")?;
            for ctr in c.nb.list(
                &format!("{CONTAINERS}/containers"),
                &[("pod", pod_id.to_string())],
            )? {
                set_down(&c.nb, "containers", &ctr)?;
            }
            set_down(&c.nb, "pods", &rec)
        })();
        if let Err(e) = r {
            errors.push(format!("pod {name}: {e}"));
        }
    }
    done(errors)
}

fn done(errors: Vec<String>) -> Result<(), String> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn pods(data: &Value) -> impl Iterator<Item = &Value> {
    data.get("pods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn set_down(nb: &Client, kind: &str, rec: &Value) -> Result<(), String> {
    if rec.get("state").and_then(Value::as_str) == Some("down") {
        return Ok(());
    }
    let rid = id(rec).ok_or("record without an id")?;
    nb.patch(
        &format!("{CONTAINERS}/{kind}"),
        rid,
        &json!({"state": "down"}),
    )
    .map(|_| ())
}

fn up_pod(c: &Ctx, place: &Placement, pod: &Value) -> Result<(), String> {
    let nb = &c.nb;
    let name = str_of(pod, "name");
    let existing = find_pod(c, place, name)?;
    let rec = nb.upsert(
        &format!("{CONTAINERS}/pods"),
        existing.as_ref(),
        &json!({
            "name": name,
            "compose_project": c.project,
            "cluster": place.cluster,
            "node": place.node,
            "state": pod_state(str_of(pod, "state")),
        }),
    )?;
    let pod_id = id(&rec).ok_or("pod without an id")?.to_string();
    let mut errors = Vec::new();

    // Interfaces and their addresses.
    let ifaces_path = format!("{CONTAINERS}/container-interfaces");
    let have = nb.list(&ifaces_path, &[("pod", pod_id.clone())])?;
    let mut keep = Vec::new();
    for iface in pod
        .get("interfaces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let ifname = str_of(iface, "name");
        keep.push(ifname.to_string());
        if let Err(e) = up_interface(c, place, &pod_id, iface, &have) {
            errors.push(format!("interface {ifname}: {e}"));
        }
    }
    for old in have
        .iter()
        .filter(|i| !keep.iter().any(|k| k == str_of(i, "name")))
    {
        if let Some(i) = id(old) {
            nb.delete(&ifaces_path, i)?;
        }
    }

    // Containers and their volumes; ones gantry no longer runs are down.
    let ctr_path = format!("{CONTAINERS}/containers");
    let have = nb.list(&ctr_path, &[("pod", pod_id.clone())])?;
    let mut keep = Vec::new();
    for ctr in pod
        .get("containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let cname = str_of(ctr, "name");
        keep.push(cname.to_string());
        if let Err(e) = up_container(c, &pod_id, ctr, &have) {
            errors.push(format!("container {cname}: {e}"));
        }
    }
    for old in have
        .iter()
        .filter(|o| !keep.iter().any(|k| k == str_of(o, "name")))
    {
        set_down(nb, "containers", old)?;
    }
    done(errors)
}

fn pod_state(s: &str) -> &str {
    match s {
        "running" | "created" | "down" => s,
        _ => "created",
    }
}

fn up_interface(
    c: &Ctx,
    place: &Placement,
    pod_id: &str,
    iface: &Value,
    have: &[Value],
) -> Result<(), String> {
    let nb = &c.nb;
    let ifname = str_of(iface, "name");
    let addrs: Vec<IpNet> = iface
        .get("addresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.as_str()?.parse().ok())
        .collect();
    let mut ip_ids = Vec::new();
    let mut network = None;
    for a in &addrs {
        let Some(prefix) = containing_prefix(nb, a.addr())? else {
            crate::log(
                &c.project,
                &format!("{ifname}: no Prefix holds {a}; the address is not recorded"),
            );
            continue;
        };
        let prefix_id = id(&prefix).ok_or("prefix without an id")?.to_string();
        ip_ids.push(ensure_ip(c, &prefix, a)?);
        if network.is_none() {
            network = container_network(nb, &prefix_id, place.cluster.as_deref())?;
        }
    }
    let existing = have.iter().find(|i| str_of(i, "name") == ifname);
    let mut body = json!({
        "pod": pod_id,
        "name": ifname,
        "enabled": true,
        "ip_addresses": ip_ids,
        "network": network,
    });
    if let Some(mac) = iface.get("mac").and_then(Value::as_str) {
        body["mac_address"] = json!(mac);
    }
    nb.upsert(
        &format!("{CONTAINERS}/container-interfaces"),
        existing,
        &body,
    )
    .map(|_| ())
}

/// The most specific Prefix that holds `ip`.
pub fn containing_prefix(nb: &Client, ip: std::net::IpAddr) -> Result<Option<Value>, String> {
    let found = nb.list("ipam/prefixes", &[("contains", ip.to_string())])?;
    Ok(found
        .into_iter()
        .max_by_key(|p| p.get("prefix_length").and_then(Value::as_u64).unwrap_or(0)))
}

/// The Container Network that carries `prefix`, preferring one in the
/// node's cluster.
fn container_network(
    nb: &Client,
    prefix: &str,
    cluster: Option<&str>,
) -> Result<Option<String>, String> {
    let path = format!("{CONTAINERS}/container-networks");
    if let Some(cl) = cluster {
        let n = nb.list(
            &path,
            &[
                ("prefixes", prefix.to_string()),
                ("cluster", cl.to_string()),
            ],
        )?;
        if let Some(first) = n.first() {
            return Ok(id(first).map(String::from));
        }
    }
    let n = nb.list(&path, &[("prefixes", prefix.to_string())])?;
    Ok(n.first().and_then(id).map(String::from))
}

/// The IP Address record for `addr` in `prefix`'s namespace, created when
/// missing.
fn ensure_ip(c: &Ctx, prefix: &Value, addr: &IpNet) -> Result<String, String> {
    let mut q = vec![("address", addr.addr().to_string())];
    if let Some(ns) = ref_id(prefix, "namespace") {
        q.push(("namespace", ns));
    }
    if let Some(ip) = c.nb.list("ipam/ip-addresses", &q)?.first() {
        return id(ip)
            .map(String::from)
            .ok_or_else(|| "address without an id".into());
    }
    let created = c.nb.post(
        "ipam/ip-addresses",
        &json!({
            "address": addr.to_string(),
            "status": {"name": c.ip_status},
            "parent": id(prefix),
            "description": format!("gantry {}", c.project),
        }),
    )?;
    id(&created)
        .map(String::from)
        .ok_or_else(|| "address without an id".into())
}

fn up_container(c: &Ctx, pod_id: &str, ctr: &Value, have: &[Value]) -> Result<(), String> {
    let nb = &c.nb;
    let name = str_of(ctr, "name");
    let image = match ctr.get("image").and_then(Value::as_str) {
        Some(i) if !i.is_empty() => Some(ensure_image(nb, i)?),
        _ => None,
    };
    let mut body = json!({
        "name": name,
        "type": match str_of(ctr, "engine") { "kiri" => "kiri", _ => "docker" },
        "pod": pod_id,
        "compose_project": c.project,
        "compose_service": str_of(ctr, "service"),
        "image": image,
        "state": container_state(str_of(ctr, "state")),
        "restart_policy": restart_policy(ctr.get("restart").and_then(Value::as_str)),
        "command": ctr.get("command").and_then(Value::as_array).map(|a| join_command(a)).unwrap_or_default(),
        "cpu_limit": ctr.get("cpus").and_then(Value::as_f64).filter(|c| *c > 0.0).map(|c| format!("{c}")),
        "memory_limit": ctr.get("mem_limit").and_then(Value::as_u64).filter(|m| *m > 0),
        "labels": ctr.get("labels").cloned().filter(Value::is_object).unwrap_or_else(|| json!({})),
    });
    let cid = str_of(ctr, "id");
    if cid.len() == 64 && cid.bytes().all(|b| b.is_ascii_hexdigit()) {
        body["container_id"] = json!(cid.to_ascii_lowercase());
    }
    let existing = have.iter().find(|h| str_of(h, "name") == name);
    let rec = nb.upsert(&format!("{CONTAINERS}/containers"), existing, &body)?;
    let rec_id = id(&rec).ok_or("container without an id")?.to_string();
    sync_volumes(nb, &rec_id, ctr)
}

/// Makes the container's volumes match its mounts, keyed by destination.
fn sync_volumes(nb: &Client, ctr_id: &str, ctr: &Value) -> Result<(), String> {
    let path = format!("{CONTAINERS}/container-volumes");
    let have = nb.list(&path, &[("container", ctr_id.to_string())])?;
    let mut keep = Vec::new();
    for m in ctr
        .get("volumes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = str_of(m, "type");
        if !matches!(kind, "volume" | "bind" | "tmpfs") {
            continue;
        }
        let dest = str_of(m, "target");
        keep.push(dest.to_string());
        let body = json!({
            "container": ctr_id,
            "type": kind,
            "name": if kind == "volume" { str_of(m, "name") } else { "" },
            "source": if kind == "tmpfs" { "" } else { str_of(m, "source") },
            "destination": dest,
        });
        let existing = have.iter().find(|h| str_of(h, "destination") == dest);
        nb.upsert(&path, existing, &body)?;
    }
    for old in have
        .iter()
        .filter(|h| !keep.iter().any(|k| k == str_of(h, "destination")))
    {
        if let Some(i) = id(old) {
            nb.delete(&path, i)?;
        }
    }
    Ok(())
}

/// The Container Image record for a reference, created when missing.
fn ensure_image(nb: &Client, reference: &str) -> Result<String, String> {
    let (repository, tag, digest) = split_image(reference);
    let path = format!("{CONTAINERS}/container-images");
    let mut q = vec![("repository", repository.clone()), ("tag", tag.clone())];
    if let Some(d) = &digest {
        q.push(("digest", d.clone()));
    }
    let found = nb.list(&path, &q)?;
    let hit = found
        .iter()
        .find(|i| str_of(i, "digest") == digest.as_deref().unwrap_or(""));
    if let Some(i) = hit.and_then(id) {
        return Ok(i.to_string());
    }
    let created = nb.post(
        &path,
        &json!({"repository": repository, "tag": tag, "digest": digest.unwrap_or_default()}),
    )?;
    id(&created)
        .map(String::from)
        .ok_or_else(|| "image without an id".into())
}

/// `repo[:tag][@digest]`. A colon after the last slash starts the tag, so
/// a registry port (`registry:5000/app`) stays in the repository.
fn split_image(reference: &str) -> (String, String, Option<String>) {
    let (rest, digest) = match reference.split_once('@') {
        Some((r, d)) => (r, Some(d.to_string())),
        None => (reference, None),
    };
    let slash = rest.rfind('/').map_or(0, |i| i + 1);
    match rest[slash..].rfind(':') {
        Some(i) => (
            rest[..slash + i].to_string(),
            rest[slash + i + 1..].to_string(),
            digest,
        ),
        None => (rest.to_string(), String::new(), digest),
    }
}

fn container_state(s: &str) -> &str {
    match s {
        "created" | "running" | "paused" | "restarting" | "removing" | "exited" | "dead"
        | "down" => s,
        _ => "created",
    }
}

fn restart_policy(r: Option<&str>) -> &'static str {
    match r.map(|r| r.split(':').next().unwrap_or(r)) {
        Some("always") => "always",
        Some("on-failure") => "on-failure",
        Some("unless-stopped") => "unless-stopped",
        _ => "no",
    }
}

/// A command line from its words, quoting the ones a shell would split.
fn join_command(words: &[Value]) -> String {
    words
        .iter()
        .filter_map(Value::as_str)
        .map(|w| {
            if !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
            {
                w.to_string()
            } else {
                format!("'{}'", w.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images() {
        let s = |r: &str| split_image(r);
        assert_eq!(s("nginx"), ("nginx".into(), "".into(), None));
        assert_eq!(s("nginx:alpine"), ("nginx".into(), "alpine".into(), None));
        assert_eq!(
            s("registry:5000/org/app:1.2"),
            ("registry:5000/org/app".into(), "1.2".into(), None)
        );
        assert_eq!(
            s("registry:5000/app"),
            ("registry:5000/app".into(), "".into(), None)
        );
        let (r, t, d) = s("ghcr.io/o/a:1@sha256:abc");
        assert_eq!(
            (r.as_str(), t.as_str(), d.as_deref()),
            ("ghcr.io/o/a", "1", Some("sha256:abc"))
        );
    }

    #[test]
    fn mappings() {
        assert_eq!(restart_policy(Some("on-failure:3")), "on-failure");
        assert_eq!(restart_policy(None), "no");
        assert_eq!(container_state("running"), "running");
        assert_eq!(container_state("weird"), "created");
        assert_eq!(pod_state("down"), "down");
        let cmd = [json!("sh"), json!("-c"), json!("echo hi; sleep 1")];
        assert_eq!(join_command(&cmd), "sh -c 'echo hi; sleep 1'");
    }
}
