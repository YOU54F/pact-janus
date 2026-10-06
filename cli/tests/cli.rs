//! Plan task 5.5: the `janus` CLI, run as a binary — `verify`, `explain` and `upgrade` against a
//! real provider on a real socket — and plan task 7.4's `check`, which is the one command whose
//! subject is a *decision* over documents rather than a running provider. Plan task 8.2 adds
//! `component push|pull`, and a verification whose component comes from an OCI registry.
//!
//! These drive the *shipped executable*, not a library function, because the surface under test is
//! the command: its flags, its output and its exit code. A CI script depends on all three, and a
//! command whose exit code is only ever asserted from inside Rust is a command nobody has run.

#[path = "../../engine/component-host/tests/support/registry.rs"]
mod registry;

use pact_janus_sample_order_service::{Config, Provider, start};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output};

fn janus(args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_janus"))
    .args(args)
    .output()
    .expect("the janus binary runs")
}

fn stdout(output: &Output) -> String {
  String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
  String::from_utf8_lossy(&output.stderr).to_string()
}

fn code(output: &Output) -> i32 {
  output.status.code().unwrap_or(-1)
}

fn repo(relative: &str) -> String {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("..")
    .join(relative)
    .canonicalize()
    .unwrap_or_else(|err| panic!("{relative}: {err}"))
    .display()
    .to_string()
}

/// The v3 pact a consumer published for the sample provider (plan task 5.4).
fn order_pact() -> String {
  repo("samples/order-service/pacts/web-app-order-service.json")
}

fn temp(name: &str) -> String {
  let mut path = std::env::temp_dir();
  path.push(format!("janus-cli-{}-{name}", std::process::id()));
  path.display().to_string()
}

/// The provider, with auth off: what a run does to earn a credential is plan task 5.3's story and
/// `hooks.rs` tells it. Here the subject is the command.
fn provider() -> Provider {
  start(Config {
    token: None,
    ..Config::default()
  })
  .expect("the sample provider binds")
}

/// The one hook every verification of this provider needs: its own v3 state endpoint. Named after
/// the provider's own port, because these tests run in parallel and each has its own provider —
/// one shared file would point every run at whichever one wrote it last.
fn state_config(provider: &Provider) -> String {
  let port = provider.base_url().rsplit(':').next().unwrap_or("0").to_string();
  let path = temp(&format!("verifier-{port}.yaml"));
  std::fs::write(
    &path,
    format!(
      "version: 1\nhooks:\n  state-setup:\n    - name: fixtures\n      run:\n        kind: http\n        url: \"{}/_pact/provider-states\"\n        format: pact-state-change\n",
      provider.base_url()
    ),
  )
  .expect("writing the config");
  path
}

fn broken_pact(name: &str) -> String {
  let path = temp(name);
  let mut document: Value =
    serde_json::from_str(&std::fs::read_to_string(order_pact()).expect("the pact")).expect("json");
  document["interactions"][0]["response"]["body"]["status"] = json!("CANCELLED");
  std::fs::write(&path, document.to_string()).expect("writing the pact");
  path
}

// ---------------------------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------------------------

#[test]
fn verify_replays_a_v3_pact_at_a_running_provider_and_exits_zero() {
  let provider = provider();
  let config = state_config(&provider);
  let output = janus(&[
    "verify",
    &order_pact(),
    "--provider-url",
    provider.base_url(),
    "--config",
    &config,
  ]);
  let text = stdout(&output);
  assert_eq!(code(&output), 0, "stdout: {text}\nstderr: {}", stderr(&output));
  assert!(
    text.contains("pact/3.0.0"),
    "the run says what it verified: {text}"
  );
  assert!(text.contains("VERIFIED: 2 verified, 0 failed"), "{text}");
}

/// A verification that ran and found mismatches is a *successful operation* whose subject failed
/// (protocol spec §10.2) — so it is exit 1, not exit 2, and the mismatch is on stdout where a
/// reader is already looking.
#[test]
fn verify_exits_one_and_names_the_position_when_the_provider_disagrees() {
  let provider = provider();
  let config = state_config(&provider);
  let pact = broken_pact("broken.json");
  let output = janus(&[
    "verify",
    &pact,
    "--provider-url",
    provider.base_url(),
    "--config",
    &config,
  ]);
  let text = stdout(&output);
  assert_eq!(code(&output), 1, "{text}");
  assert!(text.contains("FAILED  a request for an order"), "{text}");
  assert!(
    text.contains("$.response.body.status: Expected 'SHIPPED' to equal 'CANCELLED'"),
    "the mismatch names the position and both values: {text}"
  );
  assert!(
    !text.contains("executed plan"),
    "the executed plan is opt-in: {text}"
  );
}

/// `--explain-failures` is the hardening plan task 5.5 asks for: the executed plan for the variant
/// that actually failed, against the response the provider actually sent, printed where the
/// failure is — rather than a second command run against a values file written by hand.
#[test]
fn explain_failures_prints_the_executed_plan_after_the_failure_it_explains() {
  let provider = provider();
  let config = state_config(&provider);
  let pact = broken_pact("broken-explained.json");
  let output = janus(&[
    "verify",
    &pact,
    "--provider-url",
    provider.base_url(),
    "--config",
    &config,
    "--explain-failures",
  ]);
  let text = stdout(&output);
  assert_eq!(code(&output), 1, "{text}");
  let failure = text
    .find("FAILED  a request for an order")
    .expect("the failure line");
  let plan = text.find("--- executed plan").expect("the executed plan");
  assert!(plan > failure, "the evidence follows the verdict:\n{text}");
  assert!(
    text.contains("ERROR(Expected 'SHIPPED' to equal 'CANCELLED')"),
    "the executed tree carries the result at the node that produced it: {text}"
  );
  assert!(
    text.contains("$.response.body.status => 'SHIPPED'"),
    "and the value the provider actually sent: {text}"
  );
}

/// A narrowed run must be reported as narrowed (variant-semantics spec §5.1): an unreplayed
/// variant is not a passing one, and a summary that did not say so would read like one.
#[test]
fn a_filtered_run_says_so_in_its_summary() {
  let provider = provider();
  let config = state_config(&provider);
  let output = janus(&[
    "verify",
    &order_pact(),
    "--provider-url",
    provider.base_url(),
    "--config",
    &config,
    "--variant",
    "nothing-is-called-this",
  ]);
  let text = stdout(&output);
  assert!(text.contains("filtered run"), "{text}");
  assert!(text.contains("2 skipped"), "{text}");
}

#[test]
fn verify_reads_a_directory_and_skips_what_is_not_a_contract_or_a_pact() {
  let provider = provider();
  let config = state_config(&provider);
  let dir = temp("pacts");
  std::fs::create_dir_all(&dir).expect("a temp dir");
  std::fs::copy(order_pact(), format!("{dir}/web-app-order-service.json")).expect("copying the pact");
  // Filtered by identification, not by name: this one is neither, and handing it to the engine
  // would fail a run that has nothing wrong with it.
  std::fs::write(format!("{dir}/notes.json"), r#"{"hello":"world"}"#).expect("writing a stray file");

  let output = janus(&[
    "verify",
    &dir,
    "--provider-url",
    provider.base_url(),
    "--config",
    &config,
  ]);
  assert_eq!(code(&output), 0, "{}\n{}", stdout(&output), stderr(&output));
  assert!(stdout(&output).contains("VERIFIED"), "{}", stdout(&output));
}

#[test]
fn verify_json_prints_the_summary_document_a_script_can_read() {
  let provider = provider();
  let config = state_config(&provider);
  let output = janus(&[
    "verify",
    &order_pact(),
    "--provider-url",
    provider.base_url(),
    "--config",
    &config,
    "--json",
  ]);
  let summary: Value = serde_json::from_str(&stdout(&output)).expect("the summary is JSON");
  assert_eq!(summary["status"], json!("verified"));
  assert_eq!(summary["variants"]["verified"], json!(2));
}

/// A hook that aborts the run is the run's headline, so the text says *why* — here, the token
/// endpoint of a provider nobody started — rather than only that the hook failed.
#[test]
fn a_run_aborted_by_a_hook_says_what_the_hook_hit() {
  let port = std::net::TcpListener::bind("127.0.0.1:0")
    .and_then(|listener| listener.local_addr())
    .expect("a free port")
    .port();
  let url = format!("http://127.0.0.1:{port}");
  let config = temp("verifier-unreachable.yaml");
  std::fs::write(
    &config,
    format!(
      "version: 1\nhooks:\n  before-verification:\n    - name: auth\n      run: {{ kind: component, component: oauth2 }}\n      config:\n        token-url: \"{url}/oauth/token\"\n        client-id: janus-demo\n        client-secret: janus-demo-secret\n"
    ),
  )
  .expect("writing the config");
  let output = janus(&[
    "verify",
    &order_pact(),
    "--provider-url",
    &url,
    "--config",
    &config,
  ]);
  let text = stdout(&output);
  assert_eq!(code(&output), 1, "{text}\nstderr: {}", stderr(&output));
  assert!(
    text.contains("hook    auth at before-verification: failed\n          token-endpoint-unreachable:"),
    "the hook's line carries its error: {text}"
  );
  assert!(
    text.contains("aborted by the 'auth' hook at before-verification:\n  token-endpoint-unreachable:"),
    "the summary names the cause: {text}"
  );
  assert!(
    text.contains("0 skipped, 2 not run (of 2 variant(s) across 2 interaction(s))"),
    "what the abort left undone is counted, not dropped: {text}"
  );
}

// ---------------------------------------------------------------------------------------------
// explain
// ---------------------------------------------------------------------------------------------

/// The document says what it is (ADR 0011). A pact needs no flag, and the plan printed is design
/// 3.5's — the one a verification run would really execute.
#[test]
fn explain_compiles_a_pact_interaction_without_being_told_what_it_is() {
  let output = janus(&["explain", &order_pact()]);
  let text = stdout(&output);
  assert_eq!(code(&output), 0, "{}", stderr(&output));
  assert!(text.contains(":\"a request for an order\""), "{text}");
  assert!(text.contains("%match:equality"), "{text}");
  assert!(text.contains("$.response.body.status"), "{text}");
}

#[test]
fn explain_takes_the_interaction_at_an_index() {
  let output = janus(&["explain", &order_pact(), "--index", "1"]);
  assert!(
    stdout(&output).contains("a request for an order that does not exist"),
    "{}",
    stdout(&output)
  );
}

/// An interaction specification is the one document that cannot identify itself, so it is the one
/// that takes a flag — and the error for forgetting says so.
#[test]
fn explain_needs_a_flag_only_for_an_interaction_specification() {
  let spec = temp("spec.json");
  std::fs::write(
    &spec,
    json!({
      "description": "a request for an order",
      "parts": { "response": { "status": { "shape": "equality", "example": 200 } } }
    })
    .to_string(),
  )
  .unwrap();

  let refused = janus(&["explain", &spec]);
  assert_eq!(code(&refused), 2);
  assert!(
    stderr(&refused).contains("--spec"),
    "the error says what to do next: {}",
    stderr(&refused)
  );

  let output = janus(&["explain", &spec, "--spec"]);
  assert_eq!(code(&output), 0, "{}", stderr(&output));
  assert!(
    stdout(&output).contains("$.response.status"),
    "{}",
    stdout(&output)
  );
}

#[test]
fn explain_prints_the_structured_plan_document_on_request() {
  let output = janus(&["explain", &order_pact(), "--plan"]);
  let plan: Value = serde_json::from_str(&stdout(&output)).expect("the plan is JSON");
  assert_eq!(plan["grammar"], json!("v0"));
  assert_eq!(plan["root"]["kind"], json!("container"));
}

/// The offline executed form: a plan resolved against captured values, which is what a
/// golden-corpus case carries. Exit 1 when the values do not satisfy it — the same "the subject
/// failed" code `verify` uses.
#[test]
fn explain_executed_resolves_a_plan_against_captured_values() {
  let values = temp("values.json");
  std::fs::write(
    &values,
    json!({
      "$.request.method": "GET",
      "$.request.path": "/orders/66",
      "$.request.query": {},
      "$.response.status": 200,
      "$.response.headers.content-type": "application/json",
      "$.response.body": {
        "id": "66", "status": "SHIPPED", "shippedAt": "2026-07-30T09:00:00Z",
        "items": [{ "sku": "sku-0", "quantity": 1 }]
      }
    })
    .to_string(),
  )
  .unwrap();

  let output = janus(&["explain", &order_pact(), "--executed", &values]);
  let text = stdout(&output);
  assert_eq!(code(&output), 0, "{text}\n{}", stderr(&output));
  assert!(
    text.contains("=> BOOL(true)"),
    "the executed tree carries results: {text}"
  );

  let mut wrong: Value = serde_json::from_str(&std::fs::read_to_string(&values).unwrap()).unwrap();
  wrong["$.response.body"]["status"] = json!("CANCELLED");
  std::fs::write(&values, wrong.to_string()).unwrap();
  let failed = janus(&["explain", &order_pact(), "--executed", &values]);
  assert_eq!(code(&failed), 1, "{}", stdout(&failed));
  assert!(stdout(&failed).contains("ERROR("), "{}", stdout(&failed));
}

// ---------------------------------------------------------------------------------------------
// upgrade
// ---------------------------------------------------------------------------------------------

/// Canonical bytes (ADR 0018): compact, `$format` first, a trailing newline. Deterministic bytes
/// are what keep broker dedup and git diffs honest, so the command never pretty-prints.
#[test]
fn upgrade_writes_canonical_bytes_and_reports_its_findings() {
  let output = janus(&["upgrade", &order_pact()]);
  assert_eq!(code(&output), 0, "{}", stderr(&output));
  let contract = stdout(&output);
  assert!(
    contract.starts_with("{\"$format\":"),
    "the identification prefix falls out of ordinary serialization: {}",
    &contract[..contract.len().min(60)]
  );
  assert!(contract.ends_with('\n'));

  // Findings are grouped by what they mean, not listed flat. The one thing this pact loses is what
  // every HTTP request loses — its closed query, which a shape cannot close (ADR 0007).
  let findings = stderr(&output);
  assert!(findings.contains("LOSSY"), "{findings}");
  assert!(findings.contains("request-query-opened"), "{findings}");
  assert!(
    !findings.contains("generator-dropped"),
    "this pact has no generators to lose: {findings}"
  );
  assert!(findings.contains("JUDGEMENT"), "{findings}");
  assert!(findings.contains("rule-narrowed"), "{findings}");
  assert!(findings.contains("NOTE"), "{findings}");

  // One that loses more says so under the same heading: this pact carries generators, and the
  // contract format names a generator *component* nothing implements yet.
  let lossy = janus(&[
    "upgrade",
    &repo("engine/kernel/tests/fixtures/legacy-pacts/V3Consumer-ProviderStateService.json"),
  ]);
  let findings = stderr(&lossy);
  assert!(findings.contains("LOSSY"), "{findings}");
  assert!(findings.contains("generator-dropped"), "{findings}");
}

#[test]
fn upgrade_writes_to_a_file_and_can_print_one_json_document_instead() {
  let out = temp("upgraded.json");
  let output = janus(&["upgrade", &order_pact(), "--out", &out, "--quiet"]);
  assert_eq!(code(&output), 0, "{}", stderr(&output));
  let written = std::fs::read_to_string(&out).expect("the contract was written");
  assert!(written.starts_with("{\"$format\":"));

  let json = janus(&["upgrade", &order_pact(), "--json"]);
  let document: Value = serde_json::from_str(&stdout(&json)).expect("one JSON document");
  assert!(document["contract"]["interactions"].is_array());
  assert!(document["findings"].is_array());
}

/// **The migration claim, end to end through the CLI.** The same provider verifies the pact where
/// it stands (plan task 5.4) and the contract that pact upgrades into (contract-file spec §8) —
/// plan-grammar spec §4.4's two-path agreement, observed rather than asserted.
#[test]
fn a_pact_and_the_contract_it_upgrades_into_verify_the_same_provider() {
  let provider = provider();
  let config = state_config(&provider);
  let upgraded = temp("agreement.json");

  let converted = janus(&["upgrade", &order_pact(), "--out", &upgraded, "--quiet"]);
  assert_eq!(code(&converted), 0, "{}", stderr(&converted));

  for source in [order_pact(), upgraded.clone()] {
    let output = janus(&[
      "verify",
      &source,
      "--provider-url",
      provider.base_url(),
      "--config",
      &config,
    ]);
    assert_eq!(
      code(&output),
      0,
      "{source} did not verify:\n{}\n{}",
      stdout(&output),
      stderr(&output)
    );
    assert!(
      stdout(&output).contains("2 verified, 0 failed"),
      "{}",
      stdout(&output)
    );
  }
}

// ---------------------------------------------------------------------------------------------
// The command surface itself
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// check (plan task 7.4)
// ---------------------------------------------------------------------------------------------

/// The provider shape the sample provider's own tests recorded (plan task 7.2), checked in beside
/// its pacts and asserted against the recorder in `engine/kernel/tests/provider_shape_record.rs`.
fn order_shapes() -> String {
  repo("samples/order-service/shapes")
}

/// The consumer side of M5 as the RFC writes it: a contract that declares two statuses, so the
/// provider's third shows up as the RFC's own two-line finding. The pact beside the provider
/// froze one example as `equality` instead, which is a true finding about a narrower claim but
/// not the sketch.
fn order_consumer_contract() -> String {
  let path = temp("web-app.janus.json");
  let document = json!({ "$format": "janus-contract/1",
    "consumer": { "name": "web-app" },
    "provider": { "name": "order-service" },
    "interactions": [
      { "description": "a request for an order",
        "states": [ { "name": "an order exists" } ],
        "parts": { "response": { "body": {
          "shape": "object",
          "members": {
            "id": { "shape": "string", "example": "66" },
            "status": { "shape": "any-of", "options": ["PENDING", "SHIPPED"], "example": "PENDING" },
            "items": { "shape": "each-like", "min": 0,
                       "items": { "shape": "object", "members": {
                         "sku": { "shape": "string", "example": "sku-0" },
                         "quantity": { "shape": "integer", "example": 1 } } } } } } } },
        "selection": { "variants": [], "report": {} } } ] });
  std::fs::write(&path, document.to_string()).expect("writing the contract");
  path
}

fn policy_file(name: &str, body: &str) -> String {
  let path = temp(name);
  std::fs::write(&path, body).expect("writing the policy");
  path
}

/// M5, end to end and in one command: the shape the provider recorded, the contract the consumer
/// declared, and the RFC's report — reported exactly as the RFC sketches it.
#[test]
fn check_reports_the_undeclared_variance_the_way_the_rfc_sketches_it() {
  let output = janus(&[
    "check",
    &order_consumer_contract(),
    "--provider-shape",
    &order_shapes(),
  ]);
  let text = stdout(&output);
  for line in [
    "✗ web-app is not compatible with order-service",
    "  interaction 'a request for an order', response body $.status:",
    "    provider may produce: 'CANCELLED' | 'PENDING' | 'SHIPPED'",
    "    consumer has only tested: 'PENDING' | 'SHIPPED'",
  ] {
    assert!(text.contains(line), "missing {line:?} in:\n{text}");
  }
  assert_eq!(code(&output), 0, "a finding warns by default (ADR 0016): {text}");
  assert!(
    text.contains("no verification result was supplied"),
    "and a pair nobody replayed is not a passing one: {text}"
  );
}

/// The whole loop as CI would run it: verify, keep the summary, then decide with it. The policy
/// blocks on findings, so the answer is no — and the exit code says so without the command having
/// failed.
#[test]
fn check_combines_a_verification_result_with_the_findings_and_exits_one_when_blocked() {
  let provider = provider();
  let config = state_config(&provider);
  let verification = temp("verification.json");
  let verified = janus(&[
    "verify",
    &order_pact(),
    "--provider-url",
    provider.base_url(),
    "--config",
    &config,
    "--json",
  ]);
  assert_eq!(code(&verified), 0, "{}", stderr(&verified));
  std::fs::write(&verification, stdout(&verified)).expect("writing the summary");

  let policy = policy_file("policy-block.yaml", "on-finding: block\non-review: warn\n");
  let output = janus(&[
    "check",
    &order_consumer_contract(),
    "--provider-shape",
    &order_shapes(),
    "--verification",
    &verification,
    "--policy",
    &policy,
    "--as-of",
    "2026-09-22",
  ]);
  let text = stdout(&output);
  assert_eq!(code(&output), 1, "{text}\n{}", stderr(&output));
  assert!(
    text.contains("verification: verified (2 of 2 variant(s))"),
    "the verification half of the answer is on the page: {text}"
  );
  assert!(text.contains("=> BLOCK: web-app -> order-service"), "{text}");
  assert!(
    text.contains("BLOCK: 1 pair(s) blocked"),
    "and the run's own verdict: {text}"
  );
}

/// An exemption with a reason turns the same documents into a deploy — and says what it silenced
/// and until when, which is the record design 2.8 §7.2 requires.
#[test]
fn an_exemption_lets_the_same_documents_deploy_and_says_what_it_silenced() {
  let policy = policy_file(
    "policy-exempt.yaml",
    "on-finding: block\nexemptions:\n  - path: response.body.status\n    reason: \"CANCELLED ships next sprint; ORD-451\"\n    expires: \"2026-12-01\"\n",
  );
  let args = [
    "check".to_string(),
    order_consumer_contract(),
    "--provider-shape".to_string(),
    order_shapes(),
    "--policy".to_string(),
    policy,
    "--as-of".to_string(),
    "2026-09-22".to_string(),
  ];
  let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
  let output = janus(&borrowed);
  let text = stdout(&output);
  assert_eq!(code(&output), 0, "{text}");
  assert!(
    text.contains("~ interaction 'a request for an order', response body $.status:"),
    "the exempted finding is still printed, marked: {text}"
  );
  assert!(
    text.contains("exempted until 2026-12-01: CANCELLED ships next sprint; ORD-451"),
    "{text}"
  );

  // The same policy, a year later: the exemption has lapsed and the finding is decided again.
  let mut lapsed = borrowed.clone();
  let index = lapsed.len() - 1;
  lapsed[index] = "2027-09-22";
  let output = janus(&lapsed);
  let text = stdout(&output);
  assert_eq!(code(&output), 1, "{text}");
  assert!(text.contains("lapsed exemption (2026-12-01)"), "{text}");
}

#[test]
fn check_json_prints_the_compatibility_report_a_script_can_read() {
  let output = janus(&[
    "check",
    &order_consumer_contract(),
    "--provider-shape",
    &order_shapes(),
    "--json",
  ]);
  assert_eq!(code(&output), 0);
  let report: Value = serde_json::from_str(&stdout(&output)).expect("one JSON document");
  assert_eq!(report["$format"], json!("janus-compatibility-report/1"));
  assert_eq!(report["decision"], json!("warn"));
  assert_eq!(report["summary"]["pairs"], json!(1));
  assert_eq!(report["pairs"][0]["subsumption"]["verdict"], json!("no"));
  assert_eq!(report["pairs"][0]["format"], json!("janus-contract/1"));
}

/// A provider that published nothing gets today's semantics — and the command says so on stderr,
/// because "no shape was supplied for this provider" is the CLI's own half of the answer: the
/// engine was never handed a file to miss.
#[test]
fn check_without_a_provider_shape_says_which_provider_published_nothing() {
  let verification = temp("verification-noshape.json");
  let provider = provider();
  let config = state_config(&provider);
  let verified = janus(&[
    "verify",
    &order_pact(),
    "--provider-url",
    provider.base_url(),
    "--config",
    &config,
    "--json",
  ]);
  std::fs::write(&verification, stdout(&verified)).expect("writing the summary");

  let output = janus(&[
    "check",
    &order_pact(),
    "--verification",
    &verification,
    "--on-finding",
    "block",
  ]);
  let text = stdout(&output);
  assert_eq!(code(&output), 0, "replay-only semantics is a pass: {text}");
  assert!(
    text.contains("no published shape matched any interaction"),
    "{text}"
  );
  assert!(
    stderr(&output).contains("no provider shape supplied for 'order-service'"),
    "{}",
    stderr(&output)
  );
}

#[test]
fn check_refuses_a_policy_value_that_is_not_warn_or_block() {
  let output = janus(&[
    "check",
    &order_consumer_contract(),
    "--provider-shape",
    &order_shapes(),
    "--on-finding",
    "maybe",
  ]);
  assert_eq!(code(&output), 2);
  assert!(
    stderr(&output).contains("--on-finding takes 'warn' or 'block'"),
    "{}",
    stderr(&output)
  );
}

#[test]
fn an_unknown_command_or_option_is_a_command_failure_with_the_usage() {
  let unknown = janus(&["frobnicate"]);
  assert_eq!(code(&unknown), 2);
  assert!(stderr(&unknown).contains("unknown command 'frobnicate'"));
  assert!(stderr(&unknown).contains("verify"), "the usage follows");

  let bad_option = janus(&["verify", "x.json", "--nonsense"]);
  assert_eq!(code(&bad_option), 2);
  assert!(stderr(&bad_option).contains("unknown option '--nonsense'"));
}

#[test]
fn every_command_explains_itself() {
  for command in ["verify", "check", "explain", "upgrade"] {
    let output = janus(&[command, "--help"]);
    assert_eq!(code(&output), 0);
    assert!(
      stdout(&output).starts_with(&format!("usage: janus {command}")),
      "{command}: {}",
      stdout(&output)
    );
  }
  assert!(stdout(&janus(&["version"])).contains("engine protocol v"));
}

#[test]
fn verify_without_a_provider_url_is_a_usage_error_not_a_run() {
  let output = janus(&["verify", &order_pact()]);
  assert_eq!(code(&output), 2);
  assert!(
    stderr(&output).contains("missing --provider-url"),
    "{}",
    stderr(&output)
  );
}

// ---------------------------------------------------------------------------------------------
// components (plan task 8.1)
// ---------------------------------------------------------------------------------------------

/// The out-of-tree CSV component, built for `wasm32-wasip2` by the test itself so it never runs a
/// stale one. Its crate depends on nothing in this workspace.
fn csv_component() -> String {
  static WASM: std::sync::OnceLock<String> = std::sync::OnceLock::new();
  WASM
    .get_or_init(|| {
      let dir = PathBuf::from(repo("third-party/janus-csv"));
      let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()))
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .current_dir(&dir)
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("cargo runs");
      assert!(status.success(), "building the CSV component");
      dir
        .join("target/wasm32-wasip2/release/janus_csv.wasm")
        .display()
        .to_string()
    })
    .clone()
}

/// A consumer's contract for the orders export: one interaction, one recorded variant, a body the
/// interaction declares as `text/csv` (contract spec §5.5) and records as the document it decoded.
fn csv_contract() -> String {
  let path = temp("reporting.janus.json");
  let row = json!({ "id": "66", "status": "PENDING", "items": "1" });
  let document = json!({ "$format": "janus-contract/1",
    "consumer": { "name": "reporting" },
    "provider": { "name": "order-service" },
    "interactions": [
      { "description": "the orders export",
        "transport": { "kind": "http", "mode": "passive" },
        "states": [ { "name": "an order exists", "params": { "id": "66" } } ],
        "parts": {
          "request": { "method": { "shape": "equality", "example": "GET" },
                       "path": { "shape": "equality", "example": "/orders.csv" } },
          "response": { "status": { "shape": "equality", "example": 200 },
                        "body": { "shape": "each-like", "min": 1, "max": 1,
                                  "items": { "shape": "object", "members": {
                                    "id": { "shape": "string", "example": "66" },
                                    "status": { "shape": "string", "example": "PENDING" },
                                    "items": { "shape": "regex", "pattern": "^[0-9]+$", "example": "1" } } } } } },
        "content-types": { "response": { "body": "text/csv" } },
        "requires": [ { "component": "content/csv", "min-version": 1 } ],
        "selection": {
          "variants": [ { "id": "base", "origin": "base", "assignment": [],
            "states": [ { "name": "an order exists", "params": { "id": "66" } } ],
            "parts": {
              "request": { "method": { "content": "GET" }, "path": { "content": "/orders.csv" } },
              "response": { "status": { "content": 200 },
                            "body": { "content": [row], "content-type": "text/csv" } } } } ],
          "report": {} } } ] });
  std::fs::write(&path, document.to_string()).expect("writing the contract");
  path
}

/// The sample's state hook, and the component — one file, as a project writes it (lifecycle-hooks
/// spec §6.1). The component's path is relative to the file, which is how the loader resolves it.
fn csv_config(provider: &Provider, component: &str) -> String {
  let port = provider.base_url().rsplit(':').next().unwrap_or("0").to_string();
  let path = temp(&format!("verifier-csv-{port}.yaml"));
  let reference = pathdiff(component, &std::env::temp_dir());
  std::fs::write(
    &path,
    format!(
      "version: 1\ncomponents:\n  - name: csv\n    source: {{ kind: file, reference: \"{reference}\" }}\nhooks:\n  state-setup:\n    - name: fixtures\n      run:\n        kind: http\n        url: \"{}/_pact/provider-states\"\n        format: pact-state-change\n",
      provider.base_url()
    ),
  )
  .expect("writing the config");
  path
}

/// `target` relative to `base`, the way a project would write a path in a file that lives in `base`.
fn pathdiff(target: &str, base: &std::path::Path) -> String {
  let target = PathBuf::from(target);
  let base = base.canonicalize().expect("the temp dir exists");
  let common = target
    .components()
    .zip(base.components())
    .take_while(|(a, b)| a == b)
    .count();
  let ups = base.components().count() - common;
  let rest: PathBuf = target.components().skip(common).collect();
  let mut relative = PathBuf::new();
  for _ in 0..ups {
    relative.push("..");
  }
  relative.join(rest).display().to_string()
}

#[test]
fn verify_loads_a_declared_component_and_verifies_a_csv_contract() {
  let provider = provider();
  let config = csv_config(&provider, &csv_component());
  let output = janus(&[
    "verify",
    &csv_contract(),
    "--provider-url",
    provider.base_url(),
    "--config",
    &config,
  ]);
  let text = stdout(&output);
  assert_eq!(code(&output), 0, "stdout: {text}\nstderr: {}", stderr(&output));
  assert!(text.contains("VERIFIED: 1 verified, 0 failed"), "{text}");
}

#[test]
fn verify_without_the_component_fails_before_the_run_naming_what_is_missing() {
  let provider = provider();
  let config = state_config(&provider);
  let output = janus(&[
    "verify",
    &csv_contract(),
    "--provider-url",
    provider.base_url(),
    "--config",
    &config,
  ]);
  assert_eq!(code(&output), 2, "the command could not run: {}", stderr(&output));
  let text = stderr(&output);
  assert!(text.contains("component-unavailable"), "{text}");
  assert!(text.contains("content/csv"), "{text}");
}

// ---------------------------------------------------------------------------------------------
// OCI distribution (plan task 8.2)
// ---------------------------------------------------------------------------------------------

/// `janus`, with its own component cache: what is or is not already cached is the subject here.
fn janus_cached(cache: &str, args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_janus"))
    .args(args)
    .env("JANUS_COMPONENT_CACHE", cache)
    .output()
    .expect("the janus binary runs")
}

/// The sample's state hook, and the component by OCI reference and digest.
fn oci_config(provider: &Provider, reference: &str, digest: &str) -> String {
  let port = provider.base_url().rsplit(':').next().unwrap_or("0").to_string();
  let path = temp(&format!("verifier-oci-{port}.yaml"));
  std::fs::write(
    &path,
    format!(
      "version: 1\ncomponents:\n  - name: csv\n    source: {{ kind: oci, reference: \"{reference}\", digest: \"{digest}\" }}\nhooks:\n  state-setup:\n    - name: fixtures\n      run:\n        kind: http\n        url: \"{}/_pact/provider-states\"\n        format: pact-state-change\n",
      provider.base_url()
    ),
  )
  .expect("writing the config");
  path
}

#[test]
fn a_pushed_component_verifies_a_csv_contract_pinned_by_digest_and_then_offline() {
  let registry = registry::Registry::start();
  let reference = format!("{}/janus-csv:1.0.0", registry.host);
  let pushed = janus_cached(
    &temp("oci-push-cache"),
    &["component", "push", &csv_component(), &reference, "--json"],
  );
  assert_eq!(code(&pushed), 0, "{}", stderr(&pushed));
  let pushed: Value = serde_json::from_slice(&pushed.stdout).unwrap();
  assert_eq!(pushed["component"], json!({ "name": "csv", "version": "1.0.0" }));
  let digest = pushed["digest"].as_str().unwrap().to_string();

  let provider = provider();
  let config = oci_config(&provider, &reference, &digest);
  let cache = temp("oci-verify-cache");
  let _ = std::fs::remove_dir_all(&cache);
  let verify = || {
    janus_cached(
      &cache,
      &[
        "verify",
        &csv_contract(),
        "--provider-url",
        provider.base_url(),
        "--config",
        &config,
      ],
    )
  };
  registry.take_log();
  let first = verify();
  assert_eq!(
    code(&first),
    0,
    "stdout: {}\nstderr: {}",
    stdout(&first),
    stderr(&first)
  );
  assert!(
    stdout(&first).contains("VERIFIED: 1 verified, 0 failed"),
    "{}",
    stdout(&first)
  );
  assert_eq!(
    registry.take_log().len(),
    3,
    "a manifest by digest, its config, its layer"
  );

  // The pin is in the cache: the next run needs no registry at all.
  drop(registry);
  let offline = verify();
  assert_eq!(code(&offline), 0, "stderr: {}", stderr(&offline));
  assert!(stdout(&offline).contains("VERIFIED: 1 verified, 0 failed"));
}

#[test]
fn component_pull_says_what_to_pin_and_refuses_what_is_not_what_it_claims() {
  let registry = registry::Registry::start();
  let reference = format!("{}/janus-csv:1.0.0", registry.host);
  let pushed = janus_cached(
    &temp("oci-pull-push-cache"),
    &["component", "push", &csv_component(), &reference],
  );
  assert_eq!(code(&pushed), 0, "{}", stderr(&pushed));
  assert!(
    stdout(&pushed).contains("pushed csv 1.0.0 (content, matcher)"),
    "{}",
    stdout(&pushed)
  );

  let cache = temp("oci-pull-cache");
  let _ = std::fs::remove_dir_all(&cache);
  let pulled = janus_cached(&cache, &["component", "pull", &reference]);
  let text = stdout(&pulled);
  assert_eq!(code(&pulled), 0, "{}", stderr(&pulled));
  assert!(text.contains("csv 1.0.0 (content, matcher)"), "{text}");
  assert!(text.contains("content types: text/csv"), "{text}");
  assert!(
    text.contains(&format!(
      "source: {{ kind: oci, reference: \"{reference}\", digest: \"sha256:"
    )),
    "{text}"
  );

  // A registry serving other bytes: the artifact is not what it claims — the subject failed.
  registry.tamper(registry::Tamper::Blobs);
  let tampered_cache = temp("oci-pull-tampered-cache");
  let _ = std::fs::remove_dir_all(&tampered_cache);
  let tampered = janus_cached(&tampered_cache, &["component", "pull", &reference]);
  assert_eq!(code(&tampered), 1, "{}", stderr(&tampered));
  assert!(
    stderr(&tampered).contains("digest-mismatch"),
    "{}",
    stderr(&tampered)
  );

  // And no registry at all is the command failing to run.
  drop(registry);
  let unreachable = janus_cached(&tampered_cache, &["component", "pull", &reference]);
  assert_eq!(code(&unreachable), 2, "{}", stderr(&unreachable));
}

/// A broker that answers one request with `body`, and hands back the request line it was sent.
fn one_shot_broker(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
  use std::io::{BufRead, BufReader, Write};
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding a port");
  let url = format!("http://{}", listener.local_addr().expect("the bound address"));
  let handle = std::thread::spawn(move || {
    let (stream, _) = listener.accept().expect("a request");
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).expect("the request line");
    loop {
      let mut header = String::new();
      reader.read_line(&mut header).expect("a header");
      if header.trim().is_empty() {
        break;
      }
    }
    let mut stream = reader.into_inner();
    write!(
      stream,
      "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
      body.len()
    )
    .expect("the response");
    request_line
  });
  (url, handle)
}

#[test]
fn check_against_a_broker_sends_the_per_run_policy_as_query_parameters() {
  let (url, request) = one_shot_broker(
    r#"{"answer":"no","decidedBy":"matrix","pacticipant":"order-service","version":{"number":"1.0.0"},"target":{"environment":"production"},"relationships":[]}"#,
  );
  let output = janus(&[
    "check",
    "--broker",
    &url,
    "--pacticipant",
    "order-service",
    "--version",
    "1.0.0",
    "--environment",
    "production",
    "--on-finding",
    "block",
    "--on-review",
    "warn",
  ]);
  let request = request.join().expect("the broker thread");
  assert!(
    request.starts_with(
      "GET /decisions?pacticipant=order-service&version=1.0.0&environment=production&onFinding=block&onReview=warn "
    ),
    "{request}"
  );
  assert_eq!(code(&output), 1, "{}", stdout(&output));
}

#[test]
fn check_against_a_broker_still_refuses_local_documents_and_bad_policy_values() {
  let output = janus(&[
    "check",
    "--broker",
    "http://127.0.0.1:9",
    "--pacticipant",
    "a",
    "--version",
    "1",
    "--environment",
    "e",
    "--policy",
    "policy.yaml",
  ]);
  assert_eq!(code(&output), 2);
  assert!(
    stderr(&output).contains("drop the local options"),
    "{}",
    stderr(&output)
  );

  let output = janus(&[
    "check",
    "--broker",
    "http://127.0.0.1:9",
    "--pacticipant",
    "a",
    "--version",
    "1",
    "--environment",
    "e",
    "--on-review",
    "maybe",
  ]);
  assert_eq!(code(&output), 2);
  assert!(
    stderr(&output).contains("--on-review takes 'warn' or 'block'"),
    "{}",
    stderr(&output)
  );
}
