# The Pact Janus demo

Plan task **9.3**: the RFC's whole loop, against real code, in about a minute.

![The demo, recorded with VHS from demo.tape](demo.gif)

| Step | What happens | What it shows |
|---|---|---|
| 1 | `web-app`'s consumer test declares that an order's status is `PENDING` or `SHIPPED` and that it has at least one item. The engine runs the test once per variant it selects (4) and records a contract | Variant testing: a declared variation is a promise to exercise it (RFC, "A consumer test in Pact MkII") |
| 2 | `janus verify` replays each variant against the running `order-service`. Its verifier config gets an OAuth2 token, sets up each variant's provider state through the provider's existing v3 state endpoint, and adds a header with a script | Verification with hooks. Provider states are bound to variants with `whenVariant` and `variantCases` (ADR 0009) |
| 3 | `janus check` compares the contract with the shape `order-service` recorded from its own tests, and adds the verification result: the provider may send `CANCELLED`, and an order with no items | The subsumption check, which is what verification alone cannot see (RFC, "Variance the consumer never declared") |
| 4 | The consumer widens its contract to what the provider may send. The engine now selects 6 variants, and 4 of them fail the consumer's code, each named with its cause. The SDK withholds the contract | Variant testing forcing the consumer to handle what it declared |
| 5 | The consumer fixes its code, and all 6 variants pass | |
| 6 | Verification and `janus check` pass | The loop, closed |

## Running it

```sh
demo/run.sh            # press Enter between steps
demo/run.sh --pace 5   # 5 seconds between steps
demo/run.sh --ci       # no pauses; checks each step's outcome and exits non-zero if one differs (CI runs this)
```

With `--broker <url>` (combinable with the above), steps 3 and 6 go through a Pact Broker instead of
the local files: the consumer `janus publish`es its contract from a feature branch, the provider its
shape and verification result from `main`, and the consumer asks `janus check --broker ... --main-branch`.
The broker is the Rust Pact Broker ([pact_broker-rs](https://github.com/YOU54F/pact_broker-rs)), built
with its `janus` feature and run with `PACT_BROKER_ENABLE_JANUS=true`; it judges the pair with the
same engine, so the answers match the local ones. Each run publishes under versions of its own, so
one broker serves many runs.

It needs cargo, and Node 22.6 or later with npm. It builds `janus`, `janus-engine` and the sample
provider, installs the web app's one dependency (Vitest) if it is missing, and starts
`order-service` on a free port. It works on a scratch copy of the web app and applies each step's
change there, so nothing in the repo is modified.

To record the screencast again, from the repo root, with [VHS](https://github.com/charmbracelet/vhs),
`ttyd` and `ffmpeg` on the PATH:

```sh
vhs demo/demo.tape     # writes demo/demo.gif
```

VHS v0.12.0 writes no file and still exits 0. It cancels its own context before rendering, so every
ffmpeg call is killed before it starts ([charmbracelet/vhs#787](https://github.com/charmbracelet/vhs/issues/787);
fixes are open as #788–#791). Until a fixed release ships, use v0.11.0, or build v0.12.0 with
`v.Render(context.WithoutCancel(ctx))` in `evaluator.go`, which is how `demo.gif` was recorded.

## What is here

| Path | |
|---|---|
| `web-app/` | The consumer: `src/orders.ts` is the app's code, `test/orders.test.ts` its consumer test, written with the TypeScript SDK and its Vitest integration. `vitest.config.ts` points `pact-janus` at the SDK's source, because the SDK is not published; that is the only line a real project would not have |
| `steps/` | The two changes steps 4 and 5 make, as patches |
| `run.sh` | The walkthrough. It trims Vitest's stack traces and prints the SDK's own failure report; everything else on screen is the commands' own output |
| `demo.tape` | The VHS script that records `run.sh --pace 5` |

The provider side is the repo's sample provider, unchanged: `samples/order-service`, its
`verifier.janus.yaml` and hooks, and the shape its own tests recorded (`shapes/`, plan task 7.2).

## Choices worth knowing about

- **The consumer does not declare `shippedAt`.** The sample provider cannot produce a `SHIPPED`
  order without a shipping date, and it says so (`state-unavailable`). A contract that varied status
  and `shippedAt` independently would ask for exactly that combination, and the SDK has no way yet
  to exclude a combination (ADR 0008's exclusions are not in the DSL). This consumer reads only
  status and items, so it declares only those; must-ignore does the rest. The provider state binds
  `shipped` to the status (`whenVariant("status", "SHIPPED")`), which is how a consistent order gets
  set up.
- **The first `check` warns rather than blocks.** That is the default (ADR 0016). `--on-finding block`
  would make step 3 the failure instead.
