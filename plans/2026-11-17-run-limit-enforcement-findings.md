# Findings: run-limit enforcement and degenerate turns (upstream moltis)

**Status:** findings for the moltis engine. The chart half is fixed (see
`charts/moltis` — `tools.agentMaxIterations` / `tools.agentTimeoutSecs`). Nothing
below is changed by that work; each item is a behaviour the chart cannot reach.

**Raised by:** AMP #4950 (Home OS), from war-game R1 evidence —
`discoveries/war-game-r1-amber.md` G8 (T13, ~590 s) and
`discoveries/war-game-r1-jacob.md` T12 (580 s). Round-one's report said
"`max_iterations` is logged but not enforced". **That is not what the code does**,
and the correction is the most important item here.

**Household impact, stated once:** a parent asks a simple question and waits
minutes in silence. They do not retry; they stop asking. Two of R1's 32 turns
were like this.

---

## F1 — The iteration cap IS enforced; the log under-reports it by 3×

**Severity:** high (it hid the working cap from two war-game rounds, so the real
defect went unaddressed)

`crates/agents/src/runner/streaming.rs:204` and `non_streaming.rs:213` both do:

```rust
if iterations > max_iterations {
    warn!("... agent loop exceeded max iterations ({})", max_iterations);
    return Err(AgentRunError::Other(anyhow::anyhow!(
        "agent loop exceeded max iterations ({})", max_iterations)));
}
```

So the cap aborts the run. But the value compared against is **three times** the
configured base whenever `tools.registry_mode = "lazy"`:

```rust
// streaming.rs:107-116 (non_streaming.rs:119-127 is identical)
let configured_max_iterations = limits.max_iterations
    .unwrap_or(config.tools.agent_max_iterations);
let base_max_iterations = resolve_agent_max_iterations(configured_max_iterations);
// Lazy mode needs extra iterations for tool_search discovery round-trips.
let max_iterations = if config.tools.registry_mode == ToolRegistryMode::Lazy {
    base_max_iterations * 3
} else {
    base_max_iterations
};
```

Meanwhile the startup log at `crates/chat/src/run_with_tools.rs:126-132` prints
the **base**:

```
resolved agent runtime limits ... max_iterations=25 max_iterations_source=global_tools
```

Diagnosis: with the documented default (`agent_max_iterations = 25`) and lazy
mode on, an instance permits **75** iterations and logs `25`. R1 observed runs
reaching "iteration 74" and concluded the cap was unenforced. It was enforcing
75. `docs/src/configuration-reference.md:603` describes the key as "Maximum
number of agent loop iterations before aborting" and does not mention the
multiplier; `docs/src/configuration.md:82` calls it "Max tool call iterations per
run", which is also wrong in the other direction — one iteration may execute
several tool calls concurrently (`streaming.rs:799`,
`total_tool_calls += tool_calls.len()`).

**Needed:** one of —
(a) log the effective value *and* its base, e.g.
`max_iterations=75 (base 25, lazy ×3)` — cheapest, fixes the diagnosis problem
for everyone;
(b) drop the silent ×3 and make it an explicit per-mode default so configuration
means what it says;
(c) keep the multiplier but document it in `configuration-reference.md` and the
commented template at `crates/config/src/template.rs:429`.

(a) and (c) together are enough. (b) changes behaviour and would need a migration
note. This is **not** done here: the chart works around it by setting the base to
10 so the effective cap lands where the family needs it
(`charts/moltis/values.yaml`).

## F2 — No "stop searching once the answer is in hand"

**Severity:** high — this is the actual shape of Amber's T13.

T13 gathered every answer by roughly tool call #12, then ran 10+ more
`memory_search` calls plus `Read` of `/home/moltis/.moltis/agents/main/memory/…`
before replying at ~590 s. A cap bounds that tail; it does not remove it. With
the cap at 30 iterations the turn ends sooner but still spends its last ~18
iterations on searches that cannot change the answer.

`crates/agents/src/tool_loop_detector.rs` does fire on *repeated identical*
calls (`SUCCESS_REPEAT_THRESHOLD = 3`, same tool + same args), and escalates to
stripping the tool schema for one turn. That does not catch this pattern, which
is a series of **different** memory queries — no fingerprint matches.

**Needed:** a completion-aware stop. Options, in ascending cost:
(a) guidance, not engine: tell the model in `TOOLS.md` to stop gathering once it
can answer (Home OS already ships household guidance this way; a "no new
evidence, answer now" line would fit, though it competes with the agent's
existing rules);
(b) a soft budget: once the model has produced substantive answer text
(`is_substantive_answer_text`, `runner/helpers.rs:475`, ≥40 chars) and then
calls a *search-class* tool again, inject a directive rather than silently
allowing more iterations;
(c) a per-run tool-call budget distinct from the iteration budget, so iteration
count stops being a proxy for work done.
This needs an owner decision on the model-visible contract, which is why it is
filed rather than attempted.

## F3 — No bound inside a single LLM stream

**Severity:** high for the long tail; only the wall clock touches it.

Jacob's T12 took 580 s over **5 tool calls**. Nothing was looping: one
completion simply streamed for minutes. The iteration cap cannot see this (it
had barely started), and `agent_timeout_secs` is the only bound — it aborts the
whole run and returns `Timed out` with **no answer**
(`crates/chat/src/chat_error.rs:28-30`), so a slow-but-eventually-correct turn
loses its work.

Note the R1 round-one cause was partly addressed already: `max_output_tokens`
(AMP #4625, chart `defaults.llm.maxOutputTokens: 4096`) caps generated tokens per
response. That leaves reasoning tokens and provider-side queueing: on vLLM,
`max_tokens=4096` bounds output, but time-to-first-token and a slow decode rate
still stretch a single call past 300 s.

**Needed:** a *stream*-level signal rather than a run-level abort — e.g. abort
one completion after N seconds without a token and retry it, instead of killing
the run.

There is **no such knob today**, and two settings that look like one are not:

- `providers.<name>.idle_timeout_secs` (`crates/config/src/schema/providers.rs:298`)
  is "seconds of inactivity before auto-unloading local models" — it unloads a
  `local-llm` weight set from memory, consumed by the local-llm lifecycle
  manager (`crates/gateway/src/server/prepare_core/post_state.rs:445-455`). It
  never sees a chat stream.
- `providers.<name>.probe_timeout_secs` bounds availability probes only.

An OpenAI-compatible provider therefore has no per-request timeout at all; the
only thing above it is `tools.agent_timeout_secs`, which kills the run. Adding a
stream idle/response timeout to the OpenAI-compatible provider is the change
this finding asks for.

## F4 — `reasoning_effort` is inert on the OpenAI-compatible path

**Severity:** low here, but misleading (the ticket asked for it).

The task suggested capping `reasoning_effort` for the family agents.
`agents.presets.*.reasoning_effort` / `chat.reasoning_default` only take effect
where the provider implements `with_reasoning_effort`; the trait default is

```rust
// crates/agents/src/model/stream.rs:222-227
fn with_reasoning_effort(self: Arc<Self>, _effort: ReasoningEffort)
    -> Option<Arc<dyn LlmProvider>> { None }
```

The family instances run a `custom-<name>` OpenAI-compatible provider (vLLM),
which does not override it. So setting `reasoning_default` in the chart would
render a value that changes nothing — deliberately **not** added. Only the
OpenAI/Codex/Kimi-Max paths honour it (`providers/src/openai/provider/core.rs`,
`openai_codex.rs`). If Qwen reasoning depth must be controlled on vLLM, the lever
is the server (`--max-num-seqs`, `--reasoning-parser`, or the model's chat
template), not moltis config.

---

## How the chart half reaches the deployed family instances

`tools/apps/moltis-argocd.yaml` (talos dir) sets no `defaults.tools`, so it
inherits the chart defaults — no change to that manifest is needed, and none was
made (the talos dir is not a git repo, so a local edit there could be neither
reviewed nor pushed). Verified by rendering the chart with the values extracted
from that manifest's `valuesObject`: all five instances (`jacob`, `uitest`,
`amber`, `wg-jacob`, `wg-amber`) come out with `agent_max_iterations = 10`
(effective 30) and `agent_timeout_secs = 300`, on both the TOML route and the
env route.

If a future operator wants the numbers pinned in the manifest rather than
inherited, the keys are `defaults.tools.agentMaxIterations` and
`defaults.tools.agentTimeoutSecs`, beside the existing
`defaults.llm.maxOutputTokens`.

## What is already true, for the record

- `agent_max_iterations` and `agent_timeout_secs` are real, wired settings with
  semantic validation (`crates/config/src/validate/semantic.rs:357-372`) — the
  earlier claim that they were decorative is incorrect.
- `MOLTIS_TOOLS__AGENT_MAX_ITERATIONS` / `..__AGENT_TIMEOUT_SECS` env overrides
  work and are applied after the config file loads
  (`crates/config/src/loader/config_io.rs:789-905`). This is what lets a chart
  change reach an existing instance volume.
- The loop detector, the auto-continue nudge
  (`AUTO_CONTINUE_SUBSTANTIVE_TEXT_THRESHOLD = 40`) and per-iteration tool-result
  compaction all exist and bound *some* pathologies — just not F2 or F3.
