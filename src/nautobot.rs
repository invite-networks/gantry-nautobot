//! A small blocking client for the Nautobot REST API.

use crate::config::Config;
use reqwest::blocking::{Client as Http, RequestBuilder};
use serde_json::Value;
use std::time::Duration;

/// Where the invite-nautobot containers models live.
pub const CONTAINERS: &str = "plugins/invite/containers";

pub struct Client {
    base: String,
    token: String,
    http: Http,
}

impl Client {
    pub fn new(cfg: &Config) -> Result<Self, String> {
        let mut b = Http::builder()
            .timeout(Duration::from_secs(cfg.timeout))
            .user_agent(concat!("gantry-nautobot/", env!("CARGO_PKG_VERSION")));
        if let Some(ca) = &cfg.ca_file {
            let pem = std::fs::read(ca).map_err(|e| format!("ca_file {}: {e}", ca.display()))?;
            let cert = reqwest::Certificate::from_pem(&pem)
                .map_err(|e| format!("ca_file {}: {e}", ca.display()))?;
            b = b.add_root_certificate(cert);
        }
        Ok(Self {
            base: cfg.url.trim_end_matches('/').to_string(),
            token: cfg.token.clone(),
            http: b.build().map_err(|e| e.to_string())?,
        })
    }

    fn url(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        let mut u = format!("{}/api/{path}", self.base);
        if !u.ends_with('/') && !u.contains('?') {
            u.push('/');
        }
        u
    }

    fn send(&self, what: &str, req: RequestBuilder) -> Result<Value, String> {
        let resp = req
            .header("Authorization", format!("Token {}", self.token))
            .header("Accept", "application/json")
            .send()
            .map_err(|e| format!("{what}: {}", without_token(&e.to_string(), &self.token)))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            let body = without_token(&text, &self.token);
            return Err(format!("{what}: HTTP {}: {body:.400}", status.as_u16()));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| format!("{what}: {e}"))
    }

    pub fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        let what = format!("GET {path}");
        self.send(&what, self.http.get(self.url(path)).query(query))
    }

    /// Every object a list endpoint returns for `query`, following pages.
    pub fn list(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<Value>, String> {
        let mut q: Vec<(&str, String)> = query.to_vec();
        q.push(("limit", "500".into()));
        let mut page = self.get(path, &q)?;
        let mut out = Vec::new();
        loop {
            if let Some(items) = page.get("results").and_then(Value::as_array) {
                out.extend(items.iter().cloned());
            }
            match page.get("next").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => {
                    let what = format!("GET {path} (next page)");
                    page = self.send(&what, self.http.get(next))?;
                }
                _ => break,
            }
        }
        Ok(out)
    }

    /// The one object matching `query`, if any. More than one is an error,
    /// since the caller expected an identity.
    pub fn one(&self, path: &str, query: &[(&str, String)]) -> Result<Option<Value>, String> {
        let mut found = self.list(path, query)?;
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            n => Err(format!("{path}: {n} objects match {query:?}")),
        }
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let what = format!("POST {path}");
        self.send(&what, self.http.post(self.url(path)).json(body))
    }

    pub fn patch(&self, path: &str, id: &str, body: &Value) -> Result<Value, String> {
        let what = format!("PATCH {path}/{id}");
        self.send(
            &what,
            self.http
                .patch(self.url(&format!("{path}/{id}")))
                .json(body),
        )
    }

    pub fn delete(&self, path: &str, id: &str) -> Result<(), String> {
        let what = format!("DELETE {path}/{id}");
        self.send(&what, self.http.delete(self.url(&format!("{path}/{id}"))))
            .map(|_| ())
    }

    /// Creates the object, or updates the one `existing` names.
    pub fn upsert(
        &self,
        path: &str,
        existing: Option<&Value>,
        body: &Value,
    ) -> Result<Value, String> {
        match existing.and_then(|e| id(e)) {
            Some(i) => self.patch(path, i, body),
            None => self.post(path, body),
        }
    }
}

/// An object's `id`.
pub fn id(v: &Value) -> Option<&str> {
    v.get("id").and_then(Value::as_str)
}

/// The `id` of a related object, which the API returns nested.
pub fn ref_id(v: &Value, field: &str) -> Option<String> {
    match v.get(field)? {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("id").and_then(Value::as_str).map(String::from),
        _ => None,
    }
}

fn without_token(s: &str, token: &str) -> String {
    if token.is_empty() {
        s.to_string()
    } else {
        s.replace(token, "<token>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn urls_and_refs() {
        let c = Client {
            base: "https://nb".into(),
            token: "secret".into(),
            http: Http::new(),
        };
        assert_eq!(c.url("dcim/devices"), "https://nb/api/dcim/devices/");
        assert_eq!(
            c.url("/ipam/prefixes/1/available-ips"),
            "https://nb/api/ipam/prefixes/1/available-ips/"
        );
        assert_eq!(
            ref_id(&json!({"pod": {"id": "p1"}}), "pod").as_deref(),
            Some("p1")
        );
        assert_eq!(ref_id(&json!({"pod": null}), "pod"), None);
        assert_eq!(
            without_token("bad secret here", "secret"),
            "bad <token> here"
        );
    }
}
