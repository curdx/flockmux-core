# Handoff Protocol

How agents in a swarmx workspace hand work to each other: typed
`produces`/`consumes` contracts, server-minted blackboard keys, and the
`WakeCoordinator` that turns a blackboard write into a wakeup.

Unlike the M6-era convention this doc once described (archived at
[`research/handoff-protocol-m6-archive.md`](research/handoff-protocol-m6-archive.md)),
the current protocol is **enforced by the server at spawn time** — keys are
never hand-typed by agents, so the two sides of a handoff cannot drift
(the F3 bug class).

## Roles declare typed outputs and inputs

Every role manifest (`roles/*.md` front-matter, parsed in
`crates/swarmx-server/src/roles.rs`) may declare:

- `produces = ["done"]` — the typed output-kinds this role emits. Empty
  falls back to a single `done` kind at spawn time.
- `consumes = [{ from_role = "backend", kind = "done" }]` — typed upstream
  dependencies: "I consume the `kind` output of `from_role`". `kind`
  defaults to `"done"`.

The orchestrator normally leaves both alone and just passes a `role` slug to
`swarm_spawn_worker`; per-spawn `produces` / `consumes` overrides exist for
deliberate deviations.

## The server mints the handoff key

`mint_handoff_key` (`roles.rs`) is the **single source of truth** for a
handoff key:

```
<workspace_id>/<thread_slug>/<role_slug>.<kind>                  # class key
<workspace_id>/<thread_slug>/<role_slug>.<instance>.<kind>       # parallel producer
```

The **class key** (no instance token) is used for the first live producer of
that role in the direction, so a consumer spawned *before* its producer still
waits on a deterministic path. Additional same-role workers mint a unique
`<instance>` token (8 hex chars) so they cannot overwrite each other.

Both the producer's prompt injection ("write your completion summary to THIS
key, then STOP") and the consumer's resolved dependency list derive from this
one function, so producer and consumer can never disagree on the key string.
Agents never name keys themselves.

At `swarm_spawn_worker` time (`routes/rest.rs`) the server:

1. Mints one key per produced kind; the **primary handoff signal** is the
   `done` kind if present, else the first kind.
2. Resolves each `consumes` entry against the role registry
   (`resolve_consumes_to_deps`): unknown producer role → rejected with a
   did-you-mean; kind the producer doesn't declare → rejected; self-dependency
   → rejected. Typos fail LOUD at spawn, never as a silent never-wake.
   Live producers of `from_role` bind by instance (wait for all of them);
   if none are live, the class key is reserved.
3. Guards the runtime DAG: a `consumes` cycle among live workers in the
   direction is rejected (W0-4). Parallel same-role workers are allowed —
   they mint distinct instance keys. Cycle detection is keyed by handoff
   node, not role slug, so two researchers do not clobber each other in
   the graph.

## WakeCoordinator: a write becomes a wakeup

`WakeCoordinator` (`wake.rs`, one background tokio task) subscribes to the
swarm broadcast and reacts to `SwarmEvent::BlackboardChanged`:

1. **Subscription table** (`wake_subs: agent_id → Vec<blackboard_key>`).
   When any agent writes a key, every subscriber gets (a) a persisted mailbox
   note `kind="wake"` and (b) a best-effort PTY kick (`\x15…\r` — Ctrl-U +
   Enter) that wakes even a fully stopped agent, no polling.
2. **Loop closure for the orchestrator**: `swarm_spawn_worker` appends the
   new worker's primary handoff key to the *spawning* agent's subscriptions
   (`append_wake_sub`), so the orchestrator wakes the instant each worker
   finishes, reads the artifact, and updates its ledgers.
3. **A worker's own deps are NOT wake subscriptions.** Its first prompt is
   held by a readiness gate (`spawn_bootstrap_inject`, P1-D) that polls the
   blackboard and injects the bootstrap only once every dep — or its `.error`
   alias — is present. Subscribing the un-prompted worker would race the gate
   with a spurious PTY kick.
4. **Failure aliases fan out**: writing `<key>.error` or `<key>.failed` also
   wakes the base key's subscribers (`base_key_aliases`), so a failure signal
   unblocks exactly the agents waiting on the success key — no separate
   wiring. The mailbox/kick body is the **replan signal**, not "an update":
   same-role replacement spawn is allowed (new instance key). Live-stall
   TTL nudges were removed on purpose (false-positive mid-thought).
5. **Post-handoff auto-kill**: a write matching a live agent's registered
   handoff signal means that worker is done; after a 5s grace (final
   scrollback + recording flush) its PTY is torn down so the UI returns to
   ground truth.
6. **Zero-wake diagnosis**: if a written key is some agent's handoff signal
   but nothing was woken, a `depends_on`/handoff mismatch is logged (F3
   diagnostics). Broadcast-lag overflow triggers a full reconcile of
   `depends_on` against the blackboard (F12).

## exit_keys: crashed producers still fail loud

At spawn, every worker registers an `ExitKey { role, handoff_signal,
spawned_at_ms }` (`register_exit_key`). When the agent later exits — clean
`Exited` or reaper-synthesized `Error` — without a *fresh* write of its
handoff signal (a write older than `spawned_at_ms` is a leftover from a
previous run and doesn't count), the WakeCoordinator synthesizes

```
<workspace_id>/<thread_slug>/<role_slug>.<kind>.error
```

and directly wakes the subscribers of the missed signal. Downstream agents
therefore see the same `.error` whether the producer self-reported failure or
died silently (M6c step 5). Consumers' role prompts check for the `.error`
alias and route to their upstream-failed branch; they do not attempt repair —
the orchestrator decides whether to spawn a `fixer` or re-dispatch.

## Conventions that live on top

These are prompt-level conventions (in `roles/orchestrator.md`), not runtime
contracts:

- `{workspace_id}/{thread_slug}/task.ledger.md` / `progress.ledger.md` /
  `plan.json` — the orchestrator's Magentic-One dual ledger + UI checklist.
- `{workspace_id}/{thread_slug}/<role>.progress.md` — worker progress
  breadcrumbs, overwritten per milestone so the UI shows liveness.

## Why blackboard AND messages?

| Mechanism  | What it carries               | Wake semantics                                    |
| ---------- | ----------------------------- | ------------------------------------------------- |
| blackboard | Structured artifacts          | WakeCoordinator wakes subscribers / readiness gate |
| messages   | "Something happened" signals  | Stop hook (`wake-check`) → fresh turn              |

The artifact goes to the blackboard; the wake rides the event. Same pattern
as git + CI: the commit lands in the repo, the webhook wakes the consumer.
