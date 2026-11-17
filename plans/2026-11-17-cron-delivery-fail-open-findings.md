# Upstream finding: `cron` delivery verification needs engine decisions (#4952)

**Repo:** moltis (`github.com/jacobsims214/moltis`), branch `feat/mobile-ux`
**Status:** partly fixed in #4952 (tool-layer fail-closed); the remainder needs engine
decisions and is documented here rather than rewritten.
**Discipline:** same as #4950 — fix what is small and safe, file the rest precisely.

---

## The failure this came from

War-game R1 G3 (Jacob round, T15): asked for a 30-minute reminder before soccer,
the agent called moltis's local `cron` tool with `deliver_to_current_chat: true`. That
correctly failed:

```
current channel has no delivery account
```

(it was a web session, so `web_session_channel_binding()` has no `account_id`). The agent
retried with a `systemEvent` job, got `ok`, and told the parent "✅ Reminder set: 5:00 PM
tomorrow … it'll ping you". The soccer event's `reminder_minutes` was `None`: nothing could
ever fire. Three turns earlier the same agent had used `event_create reminder_minutes: [30]`
correctly, so the right path worked — the agent reached for the local scheduler and then
claimed delivery.

## What #4952 fixed (tool layer, safe)

`crates/tools/src/cron_tool.rs` — `reject_ineffective_delivery()`, called at the end of
`normalize_payload_value()` so it covers both `add` and `update`:

- `systemEvent` carrying `deliver` / `channel` / `to` / `deliver_to_current_chat` is refused.
  `CronPayload::SystemEvent { text: String }` (`crates/cron/src/types.rs:41`) has no delivery
  fields, so serde **silently discarded** them while `add()` returned the stored job as `ok`.
- `agentTurn` carrying `channel` / `to` without `deliver: true` is refused. `AgentTurn` keeps
  the destination fields but drops them when `deliver` is false, with the same silent-success
  result.

The error names the offending fields, says the job "would report success while nothing is
delivered", and names the shape that does work, so a model can correct the call.

Tests: `crates/tools/src/cron_tool/tests.rs` —
`test_add_rejects_system_event_carrying_delivery`,
`test_add_rejects_destination_without_deliver`,
`test_update_rejects_system_event_carrying_delivery`,
`test_add_still_accepts_delivery_that_can_take_effect`,
`test_description_does_not_recommend_cron_for_reminders`.
Written failing first: three of them printed the exact `ok` the R1 agent received.

Also fixed, because it was teaching the failing path: the tool's own `description()` opened
with *"For reminders and recurring tasks that should produce a response in the main
conversation … use sessionTarget: main / payload.kind: systemEvent"*. That is the exact lying
shape. Same correction in `crates/tools/src/cron_tool/schema.rs` (payload description) and
`docs/src/scheduling.md` (session-target table called `main` the reminder target).

## What still needs an engine decision

### 1. Delivery capability is never checked at create time

`crates/cron/src/service.rs:786` `validate_job_spec()` enforces shape, not reachability:

```rust
if let CronPayload::AgentTurn { deliver: true, channel, to, .. } = &job.payload {
    // only checks channel and to are non-empty strings
}
```

It never asks the channel registry whether `channel` is a **configured account**. A job with
`deliver: true, channel: "typo-account"` is accepted and fails at run time — possibly hours
later, after the user was told it was set. That is the same class of bug as the R1 failure:
accept the instruction, discover it cannot work later.

**Needs a decision because:** `CronService` has no channel-registry handle
(`crates/cron/src/service.rs`, `CronService::new`), and adding one is a constructor/DI change
across `gateway` and every test that builds a `CronService`. Options:
- (a) inject a `dyn DeliveryAccountLookup` into `CronService` and validate on `add`/`update`;
- (b) validate in the tool instead (`CronTool` is constructed in
  `crates/gateway/src/server/prepare_core/post_state.rs:691`, which does have the registry);
- (c) leave at run time, but surface it at creation through the notification stream.

(b) is the smallest and keeps `moltis-cron` free of channel knowledge, so it is the one I
would take — but it changes what `CronTool::new` needs, which is a public-ish API decision.

### 2. There is no way for an agent to ask "can I deliver from here?"

The agent's first call failed with `current channel has no delivery account`. That error is
good, but it arrives only when using `deliver_to_current_chat`. Nothing tells an agent up
front whether its session can deliver at all, so "can I reach this person's phone?" is
answered by trial and error — and after one failure, the agent guessed a different shape
instead of concluding "not from here".

**Needs a decision because:** it is a new tool/field surface (a `cron` `status` field, or a
capability in `_channel` context). `crates/chat/src/prompt.rs:400` already builds a
`_channel` context; exposing `can_deliver: bool` on it is small, but it is a contract change
for every host that builds tool context.

### 3. `maybe_deliver_cron_output` returns `Ok` for a job that never delivers

`crates/gateway/src/server/helpers.rs:741` — the first thing it does is:

```rust
if !req.deliver || delivery_text.trim().is_empty() {
    return Ok(());
}
```

So for a job with no delivery at all the run is recorded `RunStatus::Ok` with no note that
nothing was delivered. That is correct for a background job (nobody expected output), but it
is indistinguishable from a job the user was told would ping them. The run record cannot
answer "did this reach anyone?", which is what a verifier needs.

**Needs a decision because:** it is a semantics change to run records (a new
`delivered: false`/`delivery: none` field, or a distinct status). It touches the run-history
shape (`crates/cron/src/types.rs` `CronRunRecord`), the `runs` tool output, and the UI.

### 4. A `systemEvent` has no observable destination

`sessionTarget: "main"` injects via `chat.send({"text": text})` with **no** `_session_key`
(`crates/gateway/src/server/prepare_core.rs:710-720`, `SystemEventFn`). Resolution then falls
to `session_key_for(None)`, which returns `"main"`
(`crates/chat/src/service/types.rs:767`) — the default session for any caller with no
connection. So a systemEvent fired from a cron job while the user is on their phone in a web
session appears in a session nobody is looking at. It is not wrong — it is what `main` means —
but for a reminder it is the worst possible destination, and the tool used to recommend it for
exactly that.

**Needs a decision because:** making `main` "the conversation the user is currently in" is a
routing model change, not a bug fix.

## Why no Home OS change

Home OS already has a working reminder path: `event_create` / `event_update` take
`reminder_minutes`, which `apps/mcp/internal/calendar/recurrence.go:833 alarmsFor()` turns into
real VALARM entries and `remindersOf()` (`:822`) reads back. Proven in the war game (T3 used
`reminder_minutes: [30]` correctly). The bug is that the agent bypassed it, not that it is
broken.

## Recommendation

The tool-layer refusal plus the guidance change should stop the specific failure. Items 1-4
are real and worth their own tickets, but each is an engine/API decision rather than a
mechanical fix, and none of them is required to stop an agent from claiming a ping that will
not come.
