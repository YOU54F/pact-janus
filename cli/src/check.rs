//! `janus check` (plan task 7.4): the RFC's `can-i-deploy` question, answered locally.
//!
//! **It decides, it does not test.** The two inputs are documents that already exist — the
//! consumer contracts (or v1–v4 pacts) a provider has, the shapes that provider published, and
//! the verification summaries `janus verify --json` wrote — so this command needs no provider
//! running, no network and no broker. That is what makes it a CI step rather than a second test
//! run: `verify` produces evidence, `check` reads it, and a pipeline that has already verified
//! does not verify again to find out whether it may deploy.
//!
//! Three flags, three layers, in design 2.8 §7.1's own order: the specification's defaults live
//! in the engine, `--policy` is the project's document, and `--on-finding`/`--on-review` are the
//! per-run override. They are two flags rather than the plan's single `--policy warn|block`
//! because §7.1 resolves two independent questions, and "block on decided findings, warn on what
//! the checker cannot decide" — the combination ADR 0016 expects a team to move to first — has no
//! spelling in one flag.
//!
//! What this command does *not* do is merge those layers itself, or match a single exemption to a
//! single finding. Both are design 2.8 §7's semantics, and both live behind `subsumption/decide`
//! (engine-protocol spec §8.6) for the reason every other command here is thin: an SDK, a broker
//! or a GitHub Action asking the same question must get the same answer, and a policy rule
//! implemented twice is a policy rule that will eventually disagree with itself.

use crate::args::{Args, Spec};
use crate::broker::{Broker, encode};
use crate::engine;
use crate::io;
use serde_json::{Value, json};
use std::process::ExitCode;

pub const SPEC: Spec = Spec {
  values: &[
    "provider-shape",
    "verification",
    "policy",
    "on-finding",
    "on-review",
    "as-of",
    "broker",
    "pacticipant",
    "version",
    "branch",
    "environment",
    "to",
  ],
  flags: &["json", "main-branch"],
};

pub const USAGE: &str = "\
usage: janus check <contract-or-pact>... [options]
       janus check --broker <url> --pacticipant <name> (--version <v> | --branch <name>)
                   (--environment <name> | --to <tag> | --main-branch) [--as-of <date>] [--json]

  <contract-or-pact>...      consumer contracts or v1-v4 pacts, or directories of them
  --provider-shape <path>    a provider shape document, or a directory of them; repeatable
  --verification <file>      a summary from `janus verify --json`; repeatable
  --policy <file>            a subsumption policy (JSON or YAML): warn/block and exemptions
  --on-finding warn|block    override the policy, for decided findings
  --on-review warn|block     override the policy, for comparisons the checker cannot decide
  --as-of <YYYY-MM-DD>       judge exemption expiry against this date (default: today)
  --json                     print the compatibility report as JSON instead of prose

Exit 1 means the answer is no: something blocked. A warning is exit 0 — on-finding and
on-review both default to warn (ADR 0016), because a check nobody can adopt catches nothing.

This command reads documents and decides; it never contacts the provider. Verify first
(`janus verify ... --json > result.json`), then pass that result here: a pair with no
verification result is reported as unverified, which is not a pass.

With --broker the documents are the ones `janus publish` put there, and the broker decides:
its matrix (has each pact been verified?) and, with its Janus module on, this same engine over
the shape each provider version published. Exit 1 means its answer was no or unknown. The
broker applies its own policy, so --policy, --on-finding and --on-review do not apply.
Credentials come from PACT_BROKER_TOKEN, or PACT_BROKER_USERNAME and PACT_BROKER_PASSWORD.";

pub fn run(args: &Args) -> ExitCode {
  if let Some(base) = args.value("broker") {
    return run_against_broker(args, base);
  }
  let paths = args.positionals();
  if paths.is_empty() {
    return io::usage("check", "missing <contract-or-pact>", USAGE);
  }

  let mut contracts = Vec::new();
  for path in paths {
    match io::read_documents(path) {
      Ok(found) => contracts.extend(found),
      Err(err) => return io::fail(&format!("janus check: {err}")),
    }
  }
  if contracts.is_empty() {
    return io::fail(&format!(
      "janus check: no contract or pact documents found in {}",
      paths.join(", ")
    ));
  }

  let mut shapes = Vec::new();
  for path in args.values("provider-shape") {
    match io::read_provider_shapes(&path) {
      Ok(found) => shapes.extend(found),
      Err(err) => return io::fail(&format!("janus check: {err}")),
    }
  }

  let mut verifications = Vec::new();
  for path in args.values("verification") {
    match io::read_json(&path) {
      Ok(summary) => verifications.push(summary),
      Err(err) => return io::fail(&format!("janus check: {err}")),
    }
  }

  // Layer 2 (design 2.8 §7.1). The layers travel separately — the engine resolves them, because
  // "scalars override, exemptions accumulate" is the rule a host must not own (ADR 0016).
  let mut policy_layers = Vec::new();
  if let Some(path) = args.value("policy") {
    match io::read_config(path) {
      Ok(document) => policy_layers.push(document),
      Err(err) => return io::fail(&format!("janus check: {err}")),
    }
  }
  // Layer 3: the per-run override, as a policy document with only what was overridden in it.
  let mut override_layer = json!({});
  for (flag, member) in [("on-finding", "on-finding"), ("on-review", "on-review")] {
    if let Some(value) = args.value(flag) {
      if value != "warn" && value != "block" {
        return io::usage("check", &format!("--{flag} takes 'warn' or 'block'"), USAGE);
      }
      override_layer[member] = json!(value);
    }
  }
  if override_layer.as_object().is_some_and(|map| !map.is_empty()) {
    policy_layers.push(override_layer);
  }

  let as_of = args.value("as-of").map(str::to_string).unwrap_or_else(today);

  let mut engine = match engine::start() {
    Ok(engine) => engine,
    Err(err) => return io::fail(&format!("janus check: {err}")),
  };

  // One `subsumption/check` per pair the host can form, then one `subsumption/decide` over the
  // lot. The pairing is the CLI's own work — which files it was handed is exactly the knowledge
  // the engine deliberately does not have (protocol spec §8.3's "fetching from files, URLs or a
  // broker is host/CLI business").
  let mut pairs = Vec::new();
  for (path, contract) in &contracts {
    let provider = contract["provider"]["name"].as_str().unwrap_or_default();
    let consumer = contract["consumer"]["name"].as_str().unwrap_or_default();
    let shape = shapes
      .iter()
      .find(|(_, shape)| shape["provider"]["name"] == json!(provider));
    let mut pair = json!({
      "consumer": { "name": consumer },
      "provider": { "name": provider },
    });
    if let Some((shape_path, shape)) = shape {
      let result = engine::call(
        &mut engine,
        "subsumption/check",
        json!({ "contract": contract, "provider-shape": shape }),
      );
      match result {
        Ok(result) => {
          pair["subsumption"] = result["report"].clone();
          pair["format"] = result["format"].clone();
        }
        Err(err) => {
          return io::fail(&format!("janus check: {path} against {shape_path}: {err}"));
        }
      }
    }
    pairs.push(pair);
  }

  let decided = engine::call(
    &mut engine,
    "subsumption/decide",
    json!({
      "pairs": pairs,
      "verification": verifications,
      "policy": policy_layers,
      "as-of": as_of,
    }),
  );
  let decided = match decided {
    Ok(decided) => decided,
    Err(err) => return io::fail(&format!("janus check: {err}")),
  };

  if args.flag("json") {
    println!(
      "{}",
      serde_json::to_string_pretty(&decided["report"]).unwrap_or_default()
    );
  } else {
    println!("{}", decided["text"].as_str().unwrap_or_default());
    // Which shapes were found, and which were not, is the CLI's half of the answer: the engine
    // was never told a shape file existed for a provider it has no document for.
    for (path, contract) in &contracts {
      let provider = contract["provider"]["name"].as_str().unwrap_or_default();
      if !shapes
        .iter()
        .any(|(_, shape)| shape["provider"]["name"] == json!(provider))
      {
        eprintln!("janus check: no provider shape supplied for '{provider}' ({path})");
      }
    }
  }

  // A `block` is the subject failing, not the command failing (protocol spec §10.2's own
  // distinction): the check ran, and its answer is no.
  match decided["report"]["decision"].as_str() {
    Some("block") => ExitCode::from(io::SUBJECT_FAILED),
    _ => ExitCode::SUCCESS,
  }
}

/// `check --broker`: asks the broker's `GET /decisions` the question `check` answers locally.
///
/// The broker forms the pairs from what was published — which pact each provider version
/// verified, which shape it published — and asks the engine about each, so nothing here calls
/// the engine: this host's half of the answer is only the question and the page.
fn run_against_broker(args: &Args, base: &str) -> ExitCode {
  if !args.positionals().is_empty() {
    return io::usage(
      "check",
      "--broker reads published documents: name none on the command line",
      USAGE,
    );
  }
  if [
    "provider-shape",
    "verification",
    "policy",
    "on-finding",
    "on-review",
  ]
  .iter()
  .any(|option| args.value(option).is_some())
  {
    return io::usage(
      "check",
      "--broker decides with the broker's documents and policy: drop the local options",
      USAGE,
    );
  }
  let Some(pacticipant) = args.value("pacticipant") else {
    return io::usage("check", "--broker needs --pacticipant", USAGE);
  };
  let mut query = vec![("pacticipant", pacticipant.to_string())];
  match (args.value("version"), args.value("branch")) {
    (Some(version), None) => query.push(("version", version.to_string())),
    (None, Some(branch)) => query.push(("branch", branch.to_string())),
    _ => return io::usage("check", "--broker needs one of --version or --branch", USAGE),
  }
  let targets: Vec<(&str, String)> = [
    args.value("environment").map(|e| ("environment", e.to_string())),
    args.value("to").map(|t| ("to", t.to_string())),
    args
      .flag("main-branch")
      .then(|| ("mainBranch", "true".to_string())),
  ]
  .into_iter()
  .flatten()
  .collect();
  if targets.len() != 1 {
    return io::usage(
      "check",
      "--broker needs one of --environment, --to or --main-branch",
      USAGE,
    );
  }
  query.extend(targets);
  if let Some(as_of) = args.value("as-of") {
    query.push(("asOf", as_of.to_string()));
  }
  let query: Vec<String> = query
    .iter()
    .map(|(key, value)| format!("{key}={}", encode(value)))
    .collect();

  let decision = match Broker::new(base).get(&format!("/decisions?{}", query.join("&"))) {
    Ok(decision) => decision,
    Err(err) => return io::fail(&format!("janus check: {err}")),
  };
  if args.flag("json") {
    println!("{}", serde_json::to_string_pretty(&decision).unwrap_or_default());
  } else {
    println!("{}", render_decision(&decision));
  }
  match decision["answer"].as_str() {
    Some("yes") => ExitCode::SUCCESS,
    _ => ExitCode::from(io::SUBJECT_FAILED),
  }
}

/// A `/decisions` answer as prose: the answer, then each relationship it rests on, with the
/// engine's own page for each relationship it judged.
fn render_decision(decision: &Value) -> String {
  let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
  let target = &decision["target"];
  let against = if let Some(environment) = target["environment"].as_str() {
    format!("be deployed to {environment}")
  } else if let Some(tag) = target["tag"].as_str() {
    format!("be deployed alongside the versions tagged {tag}")
  } else {
    "be merged into the main branch".to_string()
  };
  let mut lines = vec![format!(
    "Can {} {} {against}? {} (decided by {})",
    text(&decision["pacticipant"]),
    text(&decision["version"]["number"]),
    text(&decision["answer"]).to_uppercase(),
    text(&decision["decidedBy"]),
  )];
  if let Some(reason) = decision["reason"].as_str() {
    lines.push(format!("  {reason}"));
  }
  for relationship in decision["relationships"].as_array().into_iter().flatten() {
    let party = |side: &str| {
      let party = &relationship[side];
      match party["version"].as_str() {
        Some(version) => format!("{} {version}", text(&party["name"])),
        None => format!("{} (no version)", text(&party["name"])),
      }
    };
    let ignored = if relationship["ignored"] == json!(true) {
      " (ignored)"
    } else {
      ""
    };
    lines.push(String::new());
    lines.push(format!(
      "{} -> {}: {} {}{ignored}",
      party("consumer"),
      party("provider"),
      text(&relationship["kind"]),
      text(&relationship["verdict"]),
    ));
    let subsumption = &relationship["subsumption"];
    if let Some(error) = subsumption["error"].as_str() {
      lines.push(format!("  subsumption could not be checked: {error}"));
    } else if subsumption["shapePublished"] == json!(false) {
      lines.push("  no provider shape published for this provider version".to_string());
    } else if let Some(page) = subsumption["text"].as_str() {
      lines.extend(page.lines().map(|line| format!("  {line}")));
    }
  }
  for notice in decision["notices"].as_array().into_iter().flatten() {
    if notice["type"] == "warning" || notice["type"] == "danger" {
      lines.push(format!("{}: {}", text(&notice["type"]), text(&notice["text"])));
    }
  }
  lines.join("\n")
}

/// Today, UTC, as `YYYY-MM-DD` — the date exemption expiry is judged against when `--as-of` says
/// nothing.
///
/// Computed here rather than read from a date library, and read here rather than in the engine:
/// the kernel has no clock on purpose (a wasm component has nothing worth trusting, and an answer
/// that depended on when it was asked would not be reproducible), so supplying the date is the
/// host's job. The algorithm is Howard Hinnant's `civil_from_days`, which is the whole of what a
/// calendar dependency would be used for here.
fn today() -> String {
  let days = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|since| since.as_secs() / 86_400)
    .unwrap_or(0) as i64;
  let (year, month, day) = civil_from_days(days);
  format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
  let z = days + 719_468;
  let era = z.div_euclid(146_097);
  let doe = z.rem_euclid(146_097);
  let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
  let year = yoe + era * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
  let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
  (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
  use super::civil_from_days;

  #[test]
  fn days_since_the_epoch_convert_to_the_date_they_are() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(1), (1970, 1, 2));
    // 2026-09-22, the day this was written, then the month boundaries either side of a
    // non-leap February and the leap day itself.
    assert_eq!(civil_from_days(20_718), (2026, 9, 22));
    assert_eq!(civil_from_days(20_512), (2026, 2, 28));
    assert_eq!(civil_from_days(20_513), (2026, 3, 1));
    assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    assert_eq!(civil_from_days(19_783), (2024, 3, 1));
  }
}
