//! A Pact Broker client: the small part of the broker's API `janus publish` and
//! `janus check --broker` speak.
//!
//! The broker is the Rust Pact Broker's MkII (pact_broker-rs, ADR D14–D16). Documents go up in two
//! steps, the way that broker stages every publication: each document's bytes to `POST /blobs`,
//! which answers with their hash, then a manifest naming the publication's type, the parties in
//! its roles and its documents by hash to `POST /pacticipants/{p}/versions/{v}/publications`. A
//! verification result goes to the classic `pb:publish-verification-results` link of the pact it
//! verified, because that is the row the broker's matrix reads. The question goes to
//! `GET /decisions`, which a broker with the Janus module on answers with the *same* engine this
//! CLI embeds, judging each relationship from the subsumption report of its pact against the shape
//! its provider published.
//!
//! The client is deliberately this file and nothing more — no HAL navigation framework, no
//! retries: what fetching from a broker is to `check` is what reading a file is to it, and the
//! CLI's commands stay the thin hosts the engine-protocol rule asks for.
//!
//! Credentials come from the environment, under the names every Pact tool already reads:
//! `PACT_BROKER_TOKEN` for a bearer token, or `PACT_BROKER_USERNAME` and `PACT_BROKER_PASSWORD`
//! for basic auth.

use base64::Engine as _;
use serde_json::Value;
use std::time::Duration;

/// A broker, at its base URL.
pub struct Broker {
  base: String,
  agent: ureq::Agent,
  authorization: Option<String>,
}

impl Broker {
  pub fn new(base: &str) -> Broker {
    let agent = ureq::Agent::config_builder()
      .http_status_as_error(false)
      .timeout_global(Some(Duration::from_secs(60)))
      .build()
      .into();
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let authorization = match (
      env("PACT_BROKER_TOKEN"),
      env("PACT_BROKER_USERNAME"),
      env("PACT_BROKER_PASSWORD"),
    ) {
      (Some(token), _, _) => Some(format!("Bearer {token}")),
      (None, Some(user), password) => {
        let pair = format!("{user}:{}", password.unwrap_or_default());
        Some(format!(
          "Basic {}",
          base64::engine::general_purpose::STANDARD.encode(pair)
        ))
      }
      _ => None,
    };
    Broker {
      base: base.trim_end_matches('/').to_string(),
      agent,
      authorization,
    }
  }

  /// `path` under the broker's base URL, or `path` itself when it is already a URL (a link the
  /// broker returned).
  fn url(&self, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
      path.to_string()
    } else {
      format!("{}{path}", self.base)
    }
  }

  /// Sends one request and reads the answer as JSON. Any status outside 2xx is an error that
  /// carries the broker's own body, because a broker's 4xx says which field it refused and why.
  pub fn send(&self, method: &str, path: &str, content_type: &str, body: &[u8]) -> Result<Value, String> {
    let url = self.url(path);
    let mut request = ureq::http::Request::builder()
      .method(method)
      .uri(&url)
      .header("accept", "application/hal+json, application/json");
    if !body.is_empty() {
      request = request.header("content-type", content_type);
    }
    if let Some(authorization) = &self.authorization {
      request = request.header("authorization", authorization);
    }
    let request = request
      .body(body)
      .map_err(|err| format!("could not build the request to {url}: {err}"))?;
    let mut response = self
      .agent
      .run(request)
      .map_err(|err| format!("{method} {url}: {err}"))?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap_or_default();
    if !(200..300).contains(&status) {
      return Err(format!("{method} {url} answered {status}: {}", text.trim()));
    }
    if text.trim().is_empty() {
      return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|err| format!("{method} {url}: {err}"))
  }

  pub fn get(&self, path: &str) -> Result<Value, String> {
    self.send("GET", path, "", &[])
  }

  pub fn post_json(&self, path: &str, body: &Value) -> Result<Value, String> {
    let bytes = serde_json::to_vec(body).expect("a Value always serializes");
    self.send("POST", path, "application/json", &bytes)
  }

  /// Stages `bytes` as a blob, answering its hash.
  pub fn upload_blob(&self, bytes: &[u8]) -> Result<String, String> {
    let answer = self.send("POST", "/blobs", "application/octet-stream", bytes)?;
    answer["hash"]
      .as_str()
      .map(str::to_string)
      .ok_or_else(|| format!("POST {}/blobs answered no hash: {answer}", self.base))
  }
}

/// `value` as one path segment or query value: everything but RFC 3986's unreserved characters
/// percent-encoded, so a branch named `feat/x` or a version `1.0.0+build` survives the trip.
pub fn encode(value: &str) -> String {
  let mut encoded = String::with_capacity(value.len());
  for byte in value.bytes() {
    match byte {
      b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => encoded.push(byte as char),
      _ => encoded.push_str(&format!("%{byte:02X}")),
    }
  }
  encoded
}

#[cfg(test)]
mod tests {
  use super::encode;

  #[test]
  fn encodes_everything_but_the_unreserved_characters() {
    assert_eq!(encode("feat/x"), "feat%2Fx");
    assert_eq!(encode("1.0.0+build"), "1.0.0%2Bbuild");
    assert_eq!(encode("web app"), "web%20app");
    assert_eq!(encode("a-b_c.d~e"), "a-b_c.d~e");
  }
}
