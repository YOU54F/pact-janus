#!/usr/bin/env bash
# The Pact Janus demo (plan task 9.3): the RFC's whole loop, against real code.
#
#   1. a consumer test records a contract, one run per variant
#   2. the provider verifies it, with its hooks: OAuth2, provider states, a script
#   3. `janus check` finds what the provider may send that the consumer never tested
#   4. the consumer widens its contract, and the new variants fail its code
#   5. the consumer fixes its code, and everything is green
#
# Usage: demo/run.sh [--pace SECONDS | --ci] [--broker URL]
#   (default)     pause for Enter between steps
#   --pace N      wait N seconds between steps instead (what demo.tape records)
#   --ci          no pauses; check every step's outcome and exit non-zero if one differs
#   --broker URL  share the documents through a Pact Broker (pact_broker-rs with its Janus module
#                 on): steps 3 and 6 `janus publish` them and ask the broker with
#                 `janus check --broker`, instead of checking the files locally
#
# Needs cargo, and Node >= 22.6 with npm. Nothing in the repo is modified: the web app is copied to a
# scratch directory and the steps are patched into the copy.

set -euo pipefail

mode=interactive
pace=0
broker=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --pace) mode=paced; pace="${2:?--pace needs a number of seconds}"; shift ;;
    --ci) mode=ci ;;
    --broker) broker="${2:?--broker needs a URL}"; shift ;;
    *) echo "usage: $0 [--pace SECONDS | --ci] [--broker URL]" >&2; exit 2 ;;
  esac
  shift
done

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
demo="$repo/demo"

bold=$'\033[1m' dim=$'\033[2m' cyan=$'\033[1;36m' green=$'\033[32m' red=$'\033[31m' reset=$'\033[0m'

step() { # a new step starts on a clean screen, except in CI, where the log should read top to bottom
  [[ "$mode" == ci ]] || printf '\033[2J\033[H'
  printf '\n%s━━ %s%s\n\n' "$cyan" "$*" "$reset"
}
say() { printf '%s%s%s\n' "$dim" "$*" "$reset"; }
typed() { printf '\n%s$ %s%s\n' "$bold" "$*" "$reset"; }
pause() {
  case "$mode" in
    interactive) printf '%s[enter]%s' "$dim" "$reset"; read -r _ ;;
    paced) sleep "$pace" ;;
    ci) ;;
  esac
}
failed=0
expect() { # expect <description> <condition...>: a CI assertion, silent when it holds
  local what="$1"; shift
  if ! "$@"; then
    printf '%s✗ demo expectation failed: %s%s\n' "$red" "$what" "$reset" >&2
    failed=1
  fi
}
contains() { grep -q -- "$2" "$1"; }

# --- setup, quietly ------------------------------------------------------------------------------

work="$(mktemp -d)"
provider_pid=""
cleanup() {
  [[ -n "$provider_pid" ]] && kill "$provider_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

say "building janus, janus-engine and the sample provider…"
(cd "$repo" && cargo build -q -p pact_janus_cli -p pact_janus_sample_order_service)
[[ -d "$demo/web-app/node_modules" ]] || (cd "$demo/web-app" && npm ci --silent --no-audit --no-fund)

export PATH="$repo/target/debug:$PATH"
export JANUS_ENGINE="$repo/target/debug/janus-engine"
export JANUS_SDK="$repo/sdks/typescript/src"

mkdir -p "$work/web-app" "$work/order-service"
cp -r "$demo/web-app/src" "$demo/web-app/test" "$demo/web-app/package.json" "$demo/web-app/vitest.config.ts" "$work/web-app/"
ln -s "$demo/web-app/node_modules" "$work/web-app/node_modules"
cp -r "$repo/samples/order-service/verifier.janus.yaml" "$repo/samples/order-service/hooks" \
  "$repo/samples/order-service/shapes" "$work/order-service/"
cd "$work"

order-service --port 0 >provider.url 2>provider.log &
provider_pid=$!
for _ in $(seq 50); do [[ -s provider.url ]] && break; sleep 0.1; done
export PROVIDER_URL CLIENT_ID=janus-demo CLIENT_SECRET=janus-demo-secret
PROVIDER_URL="$(head -1 provider.url)"
# Where the consumer test writes its contract, under a shorter name for the commands on screen.
ln -s web-app/contracts contracts
contract=contracts/web-app-order-service.janus.json
# With a broker, each run publishes under versions of its own, so one broker serves many runs.
run="$(date +%s)"
provider_version="2.0.0-$run"

# Step 3 and 6's question, locally or through the broker. On a broker the consumer publishes its
# contract from a feature branch, the provider its shape and verification result from main, and
# the consumer asks whether its version can be merged: the broker judges the pair with the same
# engine `janus check` embeds.
can_i_deploy() { # can_i_deploy <consumer version> <publish the provider's shape: yes|no>
  if [[ -z "$broker" ]]; then
    typed "janus check $contract --provider-shape order-service/shapes/ --verification verification.json"
    janus check "$contract" --provider-shape order-service/shapes/ --verification verification.json
    return
  fi
  local consumer_version="$1"
  typed "janus publish $contract --broker \$BROKER --pacticipant web-app --version $consumer_version --branch feat/orders"
  janus publish "$contract" --broker "$broker" --pacticipant web-app --version "$consumer_version" --branch feat/orders || return
  local shape=()
  [[ "$2" == yes ]] && shape=(--provider-shape order-service/shapes/order-service.provider-shape.json)
  typed "janus publish --broker \$BROKER --pacticipant order-service --version $provider_version --branch main${shape[*]+ ${shape[*]}} \\
    --verification verification.json --consumer web-app --consumer-version $consumer_version"
  janus publish --broker "$broker" --pacticipant order-service --version "$provider_version" --branch main ${shape[@]+"${shape[@]}"} \
    --verification verification.json --consumer web-app --consumer-version "$consumer_version" || return
  typed "janus check --broker \$BROKER --pacticipant web-app --version $consumer_version --main-branch"
  janus check --broker "$broker" --pacticipant web-app --version "$consumer_version" --main-branch
}

# The consumer test, with vitest's stack traces trimmed: on success, the variants the contract
# records; on failure, the SDK's own report of which variants failed and why.
consumer_test() {
  typed "npm test"
  local status=0
  (cd web-app && npm test --silent >../test.log 2>&1) || status=$?
  if [[ $status -eq 0 ]]; then
    printf '%s✓ shows an order%s\n' "$green" "$reset"
    node -e '
      const c = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
      const vs = c.interactions[0].selection.variants;
      console.log(`  the contract records ${vs.length} variants, each one exercised:`);
      for (const v of vs) {
        // the label form the engine uses: each dimension id shortened to its last path segment
        const label = v.id.replace(/response\.body\.(\w+)#\w+=/g, "$1=");
        const p = v.states[0].params;
        console.log(`    ${label.padEnd(28)} provider state: status=${p.status}, items=${p.items}`);
      }' "$contract"
  else
    printf '%s✗ shows an order%s\n' "$red" "$reset"
    awk '/VariantsFailedError/ {on = 1} on && /^ ❯/ {exit} on {print "  " $0}' test.log
    say "  …and the SDK withholds the contract: a variant this code cannot handle is not written down."
  fi
  return $status
}

# --- 1 -------------------------------------------------------------------------------------------

step "1. The consumer's test"
say "web-app shows orders from order-service. Its contract says an order's status is PENDING or"
say "SHIPPED, and it has at least one item. The provider states say how to produce each variant."
say "test/orders.test.ts:"
sed -n '/const getOrder/,/^  });/p' web-app/test/orders.test.ts
pause
status=0; consumer_test || status=$?
expect "the first consumer test passes" test $status -eq 0
pause

# --- 2 -------------------------------------------------------------------------------------------

step "2. The provider verifies it"
say "order-service is running at $PROVIDER_URL. Its verifier config gets an OAuth2 token, sets up"
say "each variant's provider state through its existing v3 state endpoint, and adds a header with a script:"
typed "grep -E '^  [a-z-]+:|name:' order-service/verifier.janus.yaml"
grep -E '^  [a-z-]+:|name:' order-service/verifier.janus.yaml
pause
typed "janus verify $contract --provider-url \$PROVIDER_URL --config order-service/verifier.janus.yaml"
status=0
janus verify "$contract" --provider-url "$PROVIDER_URL" --config order-service/verifier.janus.yaml | tee verify.log || status=$?
expect "the first verification passes" test $status -eq 0
janus verify "$contract" --provider-url "$PROVIDER_URL" --config order-service/verifier.janus.yaml --json >verification.json || true
pause

# --- 3 -------------------------------------------------------------------------------------------

step "3. Can I deploy?"
say "Verification replays what the consumer tested, so it cannot see what the provider might send"
say "instead. The provider recorded the shape of what its own tests saw it send; janus check compares"
say "the two, and adds the verification result above:"
[[ -z "$broker" ]] || say "(on the broker at $broker: each side publishes what it recorded, and the consumer asks)"
status=0
can_i_deploy "1.0.0-$run" yes | tee check.log || status=$?
expect "the first check warns" test $status -eq 0
expect "the first check finds CANCELLED" contains check.log "'CANCELLED'"
expect "the first check finds the empty list" contains check.log "an empty list"
pause

# --- 4 -------------------------------------------------------------------------------------------

step "4. The consumer widens its contract"
say "The provider may send CANCELLED, and an order with no items. The consumer says so:"
say "The change to test/orders.test.ts:"
sed -e "s/^-.*/$red&$reset/" -e "s/^+.*/$green&$reset/" "$demo/steps/1-widen-the-contract.patch"
(cd web-app && patch -s -p1 <"$demo/steps/1-widen-the-contract.patch")
pause
say "Every value the contract declares is now a variant the test must pass:"
status=0; consumer_test || status=$?
expect "the widened consumer test fails" test $status -ne 0
expect "it fails on CANCELLED" contains test.log "unknown order status 'CANCELLED'"
pause

# --- 5 -------------------------------------------------------------------------------------------

step "5. The consumer handles what the provider sends"
say "The change to src/orders.ts:"
sed -e "s/^-.*/$red&$reset/" -e "s/^+.*/$green&$reset/" "$demo/steps/2-handle-what-the-provider-sends.patch"
(cd web-app && patch -s -p1 <"$demo/steps/2-handle-what-the-provider-sends.patch")
pause
status=0; consumer_test || status=$?
expect "the fixed consumer test passes" test $status -eq 0
pause

# --- 6 -------------------------------------------------------------------------------------------

step "6. Verify and check again"
typed "janus verify $contract --provider-url \$PROVIDER_URL --config order-service/verifier.janus.yaml"
status=0
janus verify "$contract" --provider-url "$PROVIDER_URL" --config order-service/verifier.janus.yaml || status=$?
expect "the second verification passes" test $status -eq 0
janus verify "$contract" --provider-url "$PROVIDER_URL" --config order-service/verifier.janus.yaml --json >verification.json || true
pause
status=0
can_i_deploy "1.0.1-$run" no | tee check.log || status=$?
expect "the second check passes" test $status -eq 0
expect "the second check is compatible" contains check.log "is compatible with"

printf '\n%sdemo complete%s\n' "$cyan" "$reset"
exit $failed
