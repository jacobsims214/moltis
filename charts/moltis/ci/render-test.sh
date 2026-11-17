#!/bin/sh
# Render assertions for the moltis chart. No test framework in this repo, so
# this is a plain shell check against `helm template` output — run it from the
# repo root:
#
#   sh charts/moltis/ci/render-test.sh
#
# Covers AMP #4950: run limits must reach every instance by BOTH routes
# (rendered [tools] and MOLTIS_TOOLS__AGENT_* env), because the TOML alone only
# binds an empty volume while the env override binds an existing one.
set -eu

CHART=${CHART:-charts/moltis}
VALUES=${VALUES:-charts/moltis/ci/family-values.yaml}
HELM=${HELM:-helm}
fail=0

note() { printf '%s\n' "$*"; }
ok()   { printf '  ok   %s\n' "$*"; }
bad()  { printf '  FAIL %s\n' "$*"; fail=$((fail + 1)); }

render() {
    $HELM template "$@" 2>&1
}

# Instances actually rendered by the fixture: one [tools] block each.
expected_instances=$(
    render t "$CHART" -f "$VALUES" |
    grep -c '^    registry_mode = ' || true
)

note "== defaults apply to every instance =="
out=$(render t "$CHART" -f "$VALUES")

# One [tools] block per instance, with both keys.
for key in agent_max_iterations agent_timeout_secs; do
    got=$(printf '%s\n' "$out" | grep -c "^    $key = " || true)
    if [ "$got" -eq 3 ]; then
        ok "[tools].$key rendered for $got instances"
    else
        bad "[tools].$key rendered $got times, expected 3"
    fi
done

# One env var per instance for each limit.
for var in MOLTIS_TOOLS__AGENT_MAX_ITERATIONS MOLTIS_TOOLS__AGENT_TIMEOUT_SECS; do
    got=$(printf '%s\n' "$out" | grep -c -- "- name: $var" || true)
    if [ "$got" -eq 3 ]; then
        ok "$var exported for $got instances"
    else
        bad "$var exported $got times, expected 3"
    fi
done

# The rendered TOML must agree with the env var; a drift here means the two
# routes disagree and which one wins depends on load order.
toml_iter=$(printf '%s\n' "$out" | sed -n 's/^    agent_max_iterations = \(.*\)$/\1/p' | sort -u)
env_iter=$(printf '%s\n' "$out" | sed -n '/MOLTIS_TOOLS__AGENT_MAX_ITERATIONS/{n;s/^ *value: "\(.*\)"$/\1/p;}' | sort -u)
if [ -n "$toml_iter" ] && [ "$toml_iter" = "$env_iter" ]; then
    ok "iteration cap agrees across both routes ($toml_iter)"
else
    bad "iteration cap differs: toml=[$toml_iter] env=[$env_iter]"
fi
if [ "$toml_iter" -lt 1 ] 2>/dev/null; then
    bad "agent_max_iterations must be >= 1, got $toml_iter"
fi

note "== defaults are overridable per instance =="
out=$(render t "$CHART" -f "$VALUES" \
    --set instances.sam.tools.agentMaxIterations=40 \
    --set instances.sam.tools.agentTimeoutSecs=900)
if printf '%s\n' "$out" | grep -q '^    agent_max_iterations = 40$'; then
    ok "per-instance agentMaxIterations override renders"
else
    bad "per-instance agentMaxIterations override missing"
fi
if printf '%s\n' "$out" | grep -q 'value: "900"'; then
    ok "per-instance agentTimeoutSecs override reaches the env var"
else
    bad "per-instance agentTimeoutSecs override missing from env"
fi
# The other two instances keep the default.
got=$(printf '%s\n' "$out" | grep -c '^    agent_max_iterations = 10$' || true)
if [ "$got" -eq 2 ]; then
    ok "non-overridden instances keep the default"
else
    bad "expected 2 instances on the default, got $got"
fi

note "== an explicit env entry wins without duplicating the name =="
out=$(render t "$CHART" -f "$VALUES" \
    --set 'instances.sam.env[0].name=MOLTIS_TOOLS__AGENT_MAX_ITERATIONS' \
    --set 'instances.sam.env[0].value=99')
got=$(printf '%s\n' "$out" | grep -c 'MOLTIS_TOOLS__AGENT_MAX_ITERATIONS' || true)
if [ "$got" -eq 3 ]; then
    ok "no duplicate env name (3 instances, 3 occurrences)"
else
    bad "env name rendered $got times, expected 3 (duplicate or missing)"
fi
# The explicit entry may render quoted or bare depending on how it was typed.
if printf '%s\n' "$out" | grep -Eq 'value: "?99"?$'; then
    ok "explicit env entry is kept"
else
    bad "explicit env entry was dropped"
fi

note "== invalid values are rejected at render time =="
for bad_pair in "agentMaxIterations=0" "agentMaxIterations=-1" "agentTimeoutSecs=-5"; do
    if render t "$CHART" -f "$VALUES" --set "defaults.tools.$bad_pair" >/dev/null 2>&1; then
        bad "$bad_pair rendered (should fail)"
    else
        ok "$bad_pair rejected"
    fi
done

note "== 0 = no timeout is still allowed =="
if render t "$CHART" -f "$VALUES" --set defaults.tools.agentTimeoutSecs=0 |
    grep -q '^    agent_timeout_secs = 0$'; then
    ok "agentTimeoutSecs=0 renders as no timeout"
else
    bad "agentTimeoutSecs=0 should render"
fi

# Helm's own gate, as the repo uses it.
note "== helm lint =="
if $HELM lint "$CHART" -f "$VALUES" >/dev/null 2>&1; then
    ok "helm lint"
else
    bad "helm lint"
fi

note ""
if [ "$fail" -ne 0 ]; then
    note "FAILED: $fail check(s)"
    exit 1
fi
note "all render checks passed (instances rendered: $expected_instances)"
