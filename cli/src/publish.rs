//! `janus publish`: puts the documents `check` reads somewhere a pipeline can share them, a Pact
//! Broker. It is what lets `janus check --broker` answer for a consumer and a provider whose builds
//! never see each other's files.
//!
//! Three kinds of document, each published from the version that produced it:
//!
//! - a **consumer's contracts** (or v1–v4 pacts), positionally, as `pact` publications naming the
//!   provider each document itself names;
//! - a **provider's shape** (`--provider-shape`), as a `janus-provider-shape` publication;
//! - a **provider's verification result** (`--verification`, the summary `janus verify --json`
//!   wrote), recorded against the pact it verified: the one `--consumer` published at
//!   `--consumer-version`, its latest when that is absent.
//!
//! Nothing here asks the engine anything, which is why — like `component` — this command does not
//! speak the protocol: publishing a document is not an engine operation. What the broker does
//! with a shape once it has one (computing subsumption reports, judging them on `/decisions`) it
//! does with the same engine, so the answer `check --broker` gets is the one `check` would.

use crate::args::{Args, Spec};
use crate::broker::{Broker, encode};
use crate::io;
use serde_json::{Value, json};
use std::process::ExitCode;

pub const SPEC: Spec = Spec {
  values: &[
    "broker",
    "pacticipant",
    "version",
    "branch",
    "provider-shape",
    "verification",
    "consumer",
    "consumer-version",
  ],
  flags: &[],
};

pub const USAGE: &str = "\
usage: janus publish [<contract-or-pact>...] --broker <url> --pacticipant <name> --version <v> [options]

  <contract-or-pact>...      consumer contracts or v1-v4 pacts this version recorded, or
                             directories of them; each is published to the provider it names
  --broker <url>             the Pact Broker's base URL
  --pacticipant <name>       the application publishing: the consumer of its contracts, the
                             provider of its shape and verification result
  --version <v>              the application version that produced the documents
  --branch <name>            record that version on this branch first
  --provider-shape <file>    a provider shape this version recorded
  --verification <file>      a summary from `janus verify --json` this version produced...
  --consumer <name>          ...verifying this consumer's pact
  --consumer-version <v>     ...at this consumer version (default: its latest)

Credentials come from PACT_BROKER_TOKEN, or PACT_BROKER_USERNAME and PACT_BROKER_PASSWORD.

The broker is the Rust Pact Broker (pact_broker-rs); a shape is checked there only when it
runs with its Janus module on (`--enable-janus`).";

pub fn run(args: &Args) -> ExitCode {
  let fail = |message: String| io::fail(&format!("janus publish: {message}"));
  let (Some(base), Some(pacticipant), Some(version)) = (
    args.value("broker"),
    args.value("pacticipant"),
    args.value("version"),
  ) else {
    return io::usage(
      "publish",
      "--broker, --pacticipant and --version are all required",
      USAGE,
    );
  };
  let shape_path = args.value("provider-shape");
  let verification_path = args.value("verification");
  if args.positionals().is_empty() && shape_path.is_none() && verification_path.is_none() {
    return io::usage(
      "publish",
      "nothing to publish: name contracts, --provider-shape or --verification",
      USAGE,
    );
  }
  if verification_path.is_some() != args.value("consumer").is_some() {
    return io::usage(
      "publish",
      "--verification and --consumer go together: a result verifies one consumer's pact",
      USAGE,
    );
  }

  // Everything is read before anything is sent, so a typo in the last path publishes nothing.
  let mut contracts = Vec::new();
  for path in args.positionals() {
    match io::read_documents(path) {
      Ok(found) => contracts.extend(found),
      Err(err) => return fail(err),
    }
  }
  let shape = match shape_path.map(read_bytes).transpose() {
    Ok(shape) => shape,
    Err(err) => return fail(err),
  };
  let verification = match verification_path.map(io::read_json).transpose() {
    Ok(verification) => verification,
    Err(err) => return fail(err),
  };

  let broker = Broker::new(base);
  let versioned = format!(
    "/pacticipants/{}/versions/{}",
    encode(pacticipant),
    encode(version)
  );

  if let Some(branch) = args.value("branch") {
    let path = format!(
      "/pacticipants/{}/branches/{}/versions/{}",
      encode(pacticipant),
      encode(branch),
      encode(version)
    );
    if let Err(err) = broker.send("PUT", &path, "application/json", b"{}") {
      return fail(err);
    }
    println!("{pacticipant} {version} is on branch {branch}");
  }

  for (name, contract) in &contracts {
    let Some(provider) = contract["provider"]["name"].as_str() else {
      return fail(format!("{name}: names no provider"));
    };
    let bytes = serde_json::to_vec(contract).expect("a Value always serializes");
    let roles = json!({ "consumer": pacticipant, "provider": provider });
    if let Err(err) = publish(&broker, &versioned, "pact", roles, "pact", &bytes) {
      return fail(format!("{name}: {err}"));
    }
    println!("published {name}: {pacticipant} {version} -> {provider}");
  }

  if let (Some(bytes), Some(path)) = (shape, shape_path) {
    let roles = json!({ "provider": pacticipant });
    if let Err(err) = publish(
      &broker,
      &versioned,
      "janus-provider-shape",
      roles,
      "provider-shape",
      &bytes,
    ) {
      return fail(format!("{path}: {err}"));
    }
    println!("published {path}: the provider shape of {pacticipant} {version}");
  }

  if let (Some(summary), Some(consumer)) = (verification, args.value("consumer")) {
    match record_verification(
      &broker,
      pacticipant,
      version,
      consumer,
      args.value("consumer-version"),
      &summary,
    ) {
      Ok(line) => println!("{line}"),
      Err(err) => return fail(err),
    }
  }
  ExitCode::SUCCESS
}

fn read_bytes(path: &str) -> Result<Vec<u8>, String> {
  std::fs::read(path).map_err(|err| format!("{path}: {err}"))
}

/// Stages `bytes` and publishes them as the one document of a `publication_type` publication.
fn publish(
  broker: &Broker,
  versioned: &str,
  publication_type: &str,
  roles: Value,
  document: &str,
  bytes: &[u8],
) -> Result<Value, String> {
  let hash = broker.upload_blob(bytes)?;
  broker.post_json(
    &format!("{versioned}/publications"),
    &json!({
      "type": publication_type,
      "roles": roles,
      "documents": [{ "name": document, "mediaType": "application/json", "hash": hash }],
    }),
  )
}

/// Records `summary` as `provider` `version`'s verification of `consumer`'s pact.
///
/// The result's success is the summary's own `status`: `verified` and nothing else, the rule
/// `janus verify` exits by — a filtered or aborted run is not a pass.
fn record_verification(
  broker: &Broker,
  provider: &str,
  version: &str,
  consumer: &str,
  consumer_version: Option<&str>,
  summary: &Value,
) -> Result<String, String> {
  let pact_path = format!(
    "/pacts/provider/{}/consumer/{}/{}",
    encode(provider),
    encode(consumer),
    match consumer_version {
      Some(number) => format!("version/{}", encode(number)),
      None => "latest".to_string(),
    }
  );
  let pact = broker.get(&pact_path)?;
  let Some(link) = pact["_links"]["pb:publish-verification-results"]["href"].as_str() else {
    return Err(format!("{pact_path} has no pb:publish-verification-results link"));
  };
  let success = summary["status"] == "verified";
  broker.post_json(
    link,
    &json!({
      "success": success,
      "providerApplicationVersion": version,
      "verifiedBy": {
        "implementation": "pact-janus",
        "version": pact_janus_kernel::ENGINE_VERSION,
      },
    }),
  )?;
  let consumer_version = pact["_embedded"]["consumerVersion"]["number"]
    .as_str()
    .or(consumer_version)
    .unwrap_or("latest");
  Ok(format!(
    "recorded {provider} {version}'s verification of {consumer} {consumer_version}: {}",
    if success { "success" } else { "failure" }
  ))
}
