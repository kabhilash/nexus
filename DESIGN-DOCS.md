# Writing Nexus Design Docs

This doc captures lessons from earlier DD rounds. It's meant for anyone (human or LLM) drafting or revising a detailed design doc.

The goal isn't to eliminate review cycles — some issues only become visible after a first pass makes the rest of the design coherent enough to notice them. The goal is to catch the mechanical and structural problems that don't need a reviewer to find.

---

## The shape of a Nexus DD

Every DD under `docs/dd-NNN-*.md` should have:

1. **Header block** — Parent, Depends on, Referenced by, Status, Scope.
2. **Table of Contents** with anchor links.
3. **§1 Context** and **§1.1 Repo Layout** showing the crate structure.
4. **§2 Responsibilities** with explicit non-goals.
5. **§3 onwards** — component-specific sections (state machines, traits, protocols, etc.).
6. **§N Configuration** — TOML schema for any config the component consumes.
7. **§N+1 Error Handling and Observability** — fault classes + metrics table.
8. **§N+2 Testing Strategy** — unit, integration, hardware-in-loop, fault injection.
9. **§N+3 Implementation Phases** — ordered phases with exit criteria.
10. **Related Documents** block at the end.

Look at DD-002 or DD-003 for a current example. DD-001 is larger and has additional sections specific to being the foundation component.

---

## Writing the first draft

### Trait and method discipline

**Every method that gets named must have a caller, a body, and a contract.** If you mention `self.reregister_devices()` in one section, that method must be defined somewhere — even as a ~15-line pseudocode sketch. Don't name methods and move on. The pattern "I'll sketch this out later" leads to references that never get filled in and that reviewers flag.

**Every trait method needs a caller site in the doc.** If the trait has `is_connected()` but nothing in the pseudocode calls it, either it's dead weight (remove it) or the caller is missing (add it).

**Every referenced struct field must be declared.** If the code uses `self.stream`, `self.stream` must appear in a `struct Foo { ... }` block. Confusion between `self.stream` and `self.reader`/`self.writer` (a real bug caught in review) happens when the struct definition and the method body drift apart.

### Pseudocode with compile-intent

DD pseudocode isn't required to compile, but it should be realistic enough that an implementer doesn't hit structural surprises. The test is: "could I imagine writing this for real without discovering an impossibility?"

Specific patterns that tend to fail this test:

- **Borrow patterns that won't work.** Holding `&mut entry` from `device_by_path_mut(&mut self)` while calling `self.event_tx.send(...)` fights the borrow checker. The fix is to scope the mutable borrow, extract the data you need, drop the borrow, then send. If the pseudocode reads naturally left-to-right but would require a lifetime annotation battle in real code, rewrite it.
- **`&mut self` trait methods stored behind `Arc<dyn Trait>`.** If the doc writes `Arc<dyn BluezClient>` or `Arc<dyn GpsdClient>` anywhere (including in spawned-task clones), every trait method must take `&self`. `Arc` forbids exclusive access; `&mut self` requires it. If the trait genuinely needs mutation, either the storage is `Arc<Mutex<dyn T>>` (explicit) or the trait uses interior mutability internally (zbus proxies do this). Pick one and be consistent — don't write `&mut self` trait methods and then store them behind `Arc`.
- **Division without guards.** `1.0 / x` where `x` is a `u32` with no validated range is a latent divide-by-zero. Either clamp at parse time (e.g., `.max(1)`) or use a newtype like `NonZeroU32`.
- **String formatting where JSON escaping matters.** `format!(r#"{{"path":"{}"}}"#, path)` breaks on inputs with `"` or `\`. Use `serde_json::json!` or explicit escaping.
- **"Methods" that are really closures capturing self.** If a helper function needs to mutate two fields of self, it's not a free function taking `&self` — it either takes both fields as args, or is a method.
- **Cross-task state access.** If the doc describes two tokio tasks — e.g., a backend main loop and a spawned zbus Agent — and says both "store into `pending_answers`" or "read from `state_map`," that state needs an explicit sharing mechanism: `Arc<Mutex<...>>`, `Arc<RwLock<...>>`, or a command channel where one task owns the state and the other sends messages. "Both tasks access the field" is not an answer; it's a bug. Spot this by asking: for each piece of mutable state, which task owns it, and how do the other tasks talk to that owner?

When in doubt, imagine the implementer hitting an error. If the error would be "the borrow checker won't let me do this the obvious way," the pseudocode is wrong.

### Docstrings must match the code

If `parse_tpv` parses an ISO-8601 string, the `GnssFix.time` docstring can't say "gpsd reports this as a floating-point Unix epoch." Check each docstring against the code it documents. When the code changes, the docstring changes. This is the most common drift source in revisions.

### Every documented behavior needs a code path

It is easy to write "the profile has an `auto_connect: bool` flag (default true) — the device will reconnect automatically when discovered" and then never show the code that reads `auto_connect` and triggers the reconnection. The flag becomes descriptive decoration rather than controlling behavior. Readers and implementers trust that the code matches the prose; when it doesn't, the flag is effectively dead.

The discipline: every documented flag, timeout, threshold, or behavior must appear somewhere in the pseudocode. Concretely, for each item in your configuration section, profile struct, or behavior prose, verify:

- Where is this value read?
- What branch in the code does it affect?
- If the doc says "default N" for a timeout, where is the timeout actually applied?

Common failures in past DDs:
- Profile flags named but never consulted (`auto_connect`, `auto_accept_incoming`, `trusted`).
- Config fields like `discovery_device_ttl_s` referenced in an observability note but absent from the config schema.
- Metric outcomes like `outcome="timeout"` that no code path ever produces (only BlueZ-originated timeouts got translated; operator-side timeouts were invisible).
- Upstream-state fields like `Trusted` set in Nexus's local profile but never written back to the upstream daemon, producing "we think we did, they never heard us."

When the doc says X, the pseudocode proves X. No prose-only behaviors.

### Upstream contracts must match reality

When a DD describes how a daemon (BlueZ, gpsd, wpa_supplicant) behaves — the semantics of a method, the meaning of a property, the direction of a callback — that's a factual claim about an external system, not a design choice. It has to be right.

Examples of past bugs: calling a BlueZ callback "the peer sent us a passkey" when it's actually "display this passkey to the user" (opposite direction, no response). Documenting gpsd's `time` field as a float when 3.x emits ISO-8601 strings. Assuming BlueZ's Transport filter accepts `"dual"` when it only accepts `auto/bredr/le`.

These aren't mechanical errors — they're authorial confidence misplaced. Protections:

- If you're describing an upstream method, callback, or property semantics, cite the version of the upstream docs you're matching against. "BlueZ 5.66 Device1.Pair" not "BlueZ's Pair method."
- Check the actual spec or source for any non-trivial method contract. D-Bus introspection XML or `.cxx`/`.c` source is authoritative; StackOverflow threads are not.
- For methods with tricky direction (who displays / who confirms / who responds), write out the direction explicitly in the docstring. Let a reviewer check it against an upstream reference.

### State machines

**Every state must be reachable through defined transitions.** A `Pending` state that's never entered by any code path is dead. Either delete it or add the transition.

**Every transition in the diagram must exist in the code.** If the diagram shows `Pending → Acquiring`, there must be a code path that does `state = Acquiring`. Missing transitions are silent bugs that only show up when someone traces the flow.

**The diagram and the code must list the same states.** If the diagram has 4 states and the enum has 5, one of them is wrong.

**Flow diagrams (sequence-style) must match code topology.** A diagram showing "backend.pair(device) ← awaited" as a linear inline step is wrong if the code actually spawns that call into a separate task and returns immediately. When a diagram shows a blocking flow and the code is non-blocking (or vice versa), readers get confused about which is authoritative. Check every arrow in every ASCII diagram against the actual `async` / `tokio::spawn` structure in the pseudocode.

### Event bus layering

Nexus uses `tokio::sync::broadcast` for `NexusEvent`. Common mistakes:

- **Emit-then-consume loops.** If component A emits `X` and also subscribes to `X`, it sees its own emission. Either use a separate filtered variant (the two-tier pattern used for `GnssTpvReceived` / `GnssFixChanged`) or ensure the consumer and emitter are distinct components.
- **Claiming to suppress an event that's emitted upstream.** If the gpsd reader task emits `GnssSatellites` and the backend is "downstream" of it, the backend cannot suppress that event — it can only filter its own emissions. Suppression claims must match the emission point.
- **Adding a variant without updating the canonical enum.** `NexusEvent` lives in `nexus-architecture.md §6`. Every new variant goes there. Don't reference a variant that doesn't exist in the canonical list.

### Configuration

The `[section]` name in `nexus.toml` must match the component. Defaults with a `[section.defaults]` subtable, overridable per-device/per-profile, is the standard pattern — see DD-005 §8.

Any config field referenced in pseudocode must appear in the TOML schema. A `acquisition_timeout_s` mentioned in §3 but missing from the `[gnss]` table is a gap reviewers flag.

### Metrics

Follow the conventions in DD-001 §9.5:

- `nexus_<component>_<metric>_<unit>` naming.
- Label cardinality note for any high-cardinality label (`device_path`, `ssid_hash`, etc.).
- Every failure path in the pseudocode should have a corresponding counter increment. If there's a "suppressed by emission policy" branch, there's a `nexus_*_suppressed_total` counter.

### Profile Store integration

If your component stores profiles (Ethernet, Wi-Fi, GNSS, Bluetooth):

- Pick a key scheme: functional key (ifname, ssid_hash) or ULID. State the reason. USB-unstable paths → ULID; stable identifiers → functional key.
- The on-disk filename derives from the key.
- Every stored profile has a ULID for D-Bus path stability regardless of the filename scheme.
- Add methods to the `ProfileStore` trait in DD-007; they're additive. List them in your DD.
- Add a variant to `ProfileRef` if the component needs keyed lookups (see DD-007 §5.1).
- Credentials go in a separate `*Secrets` struct with `SecretString` fields — never `String`. See DD-003 for the dual-struct pattern.

---

## Before presenting a draft

Five passes that catch most drift and oversight issues. Passes 1–3 are mechanical; 4 and 5 are structural and require thinking with the doc in front of you.

### Pass 1 — Name sweep

For every term that has a canonical spelling (type names, event variants, method names, state names), grep the doc and verify every hit. Specifically:

- After any rename (e.g., `Fix` → `GnssFix`), grep the old name across the full doc. Every remaining hit is either a mistake or needs an explicit note.
- After dropping a state or variant, grep the dropped name. Remaining hits are dead references.
- After adding a new method or variant, grep to ensure you haven't introduced two callers with different spellings.

This catches ~30% of review issues on its own.

### Pass 2 — End-to-end trace

Pick the primary happy-path flow and walk it sentence-by-sentence through every section. For DD-005 the happy path is: "fresh boot → GNSS device discovered → gpsd comes up → TPV arrives → quality filter passes → D-Bus client sees FixChanged."

At each step, check:

- Does the previous step produce the input this step expects?
- Does this step's output match the next step's input?
- Is every named component actually defined somewhere in the doc?
- Does the flow cross any section boundaries? If so, do the sections agree?

For designs with more than one tokio task (a zbus Agent separate from the backend main loop, a reader task separate from its supervisor, a driver task spawned for blocking work), the trace must also walk every **cross-task boundary**:

- Which task owns each piece of mutable state?
- When task A needs to modify state owned by task B, how does it communicate that? (Command channel, `Arc<Mutex>`, oneshot, something else?)
- Is that communication mechanism actually shown in the pseudocode, or only described in prose?
- If the primary path requires task A to register a oneshot that task B will resolve, is there concrete code for both the registration and the resolution? Or is the registration handwaved?

For DD-004, the primary path crossed four tasks: the D-Bus method handler, the backend main loop, a spawned pair-driver task, and the BlueZ Agent zbus task. Each cross-task boundary was a potential gap. DESIGN-DOCS.md's early passes didn't force an explicit enumeration of these boundaries, so several were under-specified. On a multi-task design, the trace should produce a table like:

| Task | Owns state | Receives from | Sends to |
|---|---|---|---|
| Backend main loop | `adapters`, `devices`, `pending_prompt_answers` | `cmd_rx`, `event_rx` | `event_tx` |
| Pair driver (per-job) | — | — | `event_tx` |
| Agent (zbus server) | — | BlueZ method invocations | `cmd_tx` (to register oneshot) |
| D-Bus method handlers | — | D-Bus method calls | `cmd_tx` |

If you can't fill in this table without introducing new machinery, the design has a gap that needs closing before the first draft is presentable.

This catches "two sections each sensible alone" mismatches — the single biggest source of review issues after name drift.

### Pass 3 — Referenced-symbol check

For every method call, trait method, struct field, config key, event variant, metric name, and error variant mentioned anywhere in the doc, verify there's a definition site. If the definition site is "handwaved in prose," either add a pseudocode sketch or remove the reference.

The check goes both ways:

- **Every referenced symbol has a definition.** `self.reregister_devices()` without a body is a gap.
- **Every defined symbol has a reference.** A trait method like `cancel_pairing` or `set_trusted` that's declared but never called from any code path in the DD is dead weight — either there's a missing caller (and the happy path is incomplete), or the method is genuinely unused (and should be removed from the trait). Dead trait methods are a strong signal of an unfinished flow.

This catches ~80% of "Method X is never defined" and "Field Y never appears in the struct" issues.

### Pass 4 — Failure-mode enumeration (before design, not after)

`§11.1 Fault Classes` exists to enumerate what can go wrong. It's easy to write this section descriptively — "here's what happens when gpsd restarts" — after the design is done. The more useful version is to write it *first*, before designing the happy path, and let the failure modes shape the design.

Questions to ask early:

- What does the upstream dependency do when it crashes, restarts, upgrades, or hangs?
- What happens when the config is degenerate (zero rates, empty lists, missing fields)?
- What happens when the data is degenerate (missing fields, nulls, out-of-range values)?
- What happens when a downstream consumer is slow or disconnected?
- What happens when two concurrent events arrive in either order?

A design that handles the failure modes explicitly is almost always cleaner than a design that handles the happy path elegantly and then bolts on error handling.

### Pass 5 — Cross-doc contract check

A DD rarely stands alone. It references types from `nexus-core`, profile methods from DD-007, D-Bus interfaces in DD-006, NexusEvent variants in the architecture doc. Each reference is a contract with another doc, and contracts break when one side updates without the other.

Grep the full doc for references to other docs. For each one:

- The referenced section / type / method exists in the target doc.
- Field shapes match: if this DD says `LastFix` is a 10-tuple, DD-006 should agree.
- Forward references to sections that don't exist yet (e.g., "DD-006 §6.6") are flagged as either "will be added" (create the placeholder in the target doc now) or removed entirely.

Past bugs caught too late:
- DD-005 referenced a `LastFix` tuple that differed in field count from DD-006's declaration.
- DD-004 forward-references `DD-006 §6.6` (fi.nexus.BluetoothDevice) that doesn't exist yet in DD-006.
- DD-001 references an undefined `BtAddr` type that no DD defines.

When a cross-doc check finds a gap, the fix often belongs in both docs, not just this one. Add placeholders in the target doc if needed rather than letting the reference dangle.

---

## During review

### Structure issues to flag first

When reviewing a DD, scan for these structural issues first — they have the highest reviewer:drafter leverage:

1. **State machine dead ends.** States the code can enter but never leave, or transitions in the diagram with no code path.
2. **Event emission layering.** Component claims to emit/filter/suppress an event it doesn't own.
3. **Trait method / caller mismatches.** Methods on traits nobody calls, or calls to methods nobody defined.
4. **Struct fields referenced but not declared.** Usually a sign of an incomplete refactor.
5. **Cross-doc contract violations.** DD-005 references `fi.nexus.Gnss.LastFix` with 9 fields; DD-006 defines it with 8. This is where doc-spanning reviews pay for themselves.

### Stylistic issues to flag last

Doc structure, heading casing, version-string consistency, prose improvements — these are real but low-leverage. Flag them but don't let them dominate the review. A doc with clean structure and messy headings is in better shape than one with clean headings and a state-machine dead end.

### Severity labels

Use three buckets when reviewing:

- **Must fix (correctness).** The design won't work, or the code implementing it will have a bug. Examples: state machine dead ends, type-name mismatches, division-by-zero, JSON injection.
- **Should fix (clarity/consistency).** The design works, but a reader or implementer will be confused. Examples: stale docstrings, missing trait methods, incomplete diagrams, ambiguous field semantics.
- **Worth considering (gaps).** The design works for the enumerated cases but has a corner case not addressed. Examples: handling of new upstream message types, metrics for a new failure mode, operator workflows for device replug.

Stylistic items go in a fourth bucket that's optional to apply.

---

## The meta-pattern: iterative design is fine

A DD doesn't have to be right on the first draft. Two-pass review is expected. The goal of the first draft is to be coherent enough that the second pass can find real issues rather than mechanical ones.

Rough rule of thumb from past DDs: if the re-review finds more than ~10% of its items as "genuine new insight," the first draft was probably too rushed. If it finds more than ~50% as "drift from earlier fixes," the fix-application discipline needs work. A healthy review cycle has most items in "unstated assumptions" and "pseudocode realism" — things that only become visible in the draft, and that structured passes can catch next time.

### Where the passes do and don't help

The passes catch mechanical errors — stale names after renames, methods without bodies, states with no transitions, fields that don't exist. They're force-multipliers for simple structural problems.

They do not eliminate review cycles for complex designs. DD-005 (GNSS) had a relatively simple topology: one reader task, one backend task, one type of data flowing through. Its first review found 33 items, most of them mechanical. DD-004 (Bluetooth) had four concurrent tasks, multi-stage async callbacks, and interior-mutability concerns stacked on top. Its first review, after the same three passes, found 34 items — but most of them were genuine design questions, not mechanical errors.

The lesson: complexity of the subject matter drives the review count more than the discipline does. A four-task design with async callbacks and shared state will have genuine architectural questions that only surface when the pieces are drafted and reviewed in context. No amount of pre-presentation checking will eliminate them, because they depend on seeing the whole design at once.

What the discipline *does* do: it shifts the review distribution. With the passes, mechanical errors drop from ~50% of items to ~10%, leaving the review to focus on actual design concerns. That's a meaningful improvement even when the absolute count doesn't fall.

### When to invoke which pass

Not every DD needs every pass. Use judgment:

- **Always do Pass 1 (name sweep)**, especially after any refactor pass that renames types, drops states, or moves code between sections. It's cheap and catches a predictable class of mistakes.
- **Pass 2 (end-to-end trace) matters most for multi-component designs.** A DD with one state machine on one task benefits less. A DD with coordinating tasks, async callbacks, or multi-stage protocols benefits enormously.
- **Pass 3 (referenced-symbol check) is always worth doing.** It's the single highest-leverage pass — catches both undefined references and dead code in one sweep.
- **Pass 4 (failure-mode enumeration) is best done before the main design, not as a check afterward.** Failure modes that show up in the design as first-class branches are always cleaner than ones retrofitted.
- **Pass 5 (cross-doc contracts) matters when the DD depends on or is depended on by others.** For a standalone component, skip it. For a DD that adds methods to the Profile Store trait, defines D-Bus interfaces in DD-006, or consumes NexusEvent variants, it's non-negotiable.
