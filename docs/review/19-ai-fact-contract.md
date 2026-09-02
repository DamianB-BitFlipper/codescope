# Review 19 — AI fact contract after lazy analysis

Reviewed `HEAD` `7900133` and the lazy-analysis change `0c6e7da` statically. I did
not run Cargo tests. No source file was changed as part of this review.

## Executive finding

The provider and JSON parser are not the primary failure. The request advertises a
semantic planner while supplying a Git-only validation universe.

On the common 83-file cold path (the user presses `A` before expanding a file), there
are zero `Ready` file entries. Consequently:

- the digest has zero changed symbols, zero semantic diagnostics, and zero relations;
  it has only the capped hunk tier and repo sketch;
- `SnapshotFacts.symbols` is empty;
- `SnapshotFacts.edges` is empty; and
- all nine read tools are advertised even though every execution uses
  `NoToolExecutor` and fails.

**The single dominant form-killing failure is symbol resolution against the empty lazy
symbol set.** A normal entity-backed semantic plan reaches node validation before edge
validation. Every symbol is interpreted as “does not resolve”; a flow rejects at its
first invalid endpoint, a symbol tree rejects at an invalid root or at the 20% rule, and
a list loses all bullets. Empty edge facts are a second guaranteed failure for a
relationship form whose endpoints happen to be file-level or otherwise resolve.

This is a request/fact capability mismatch introduced by `0c6e7da`, not evidence that
the cited symbols or relationships are absent. Before that commit, `spawn_ai` used the
eager `AnalysisSnapshot` digest and `SnapshotFacts::new`; after it, the implementation
changed to the changeset plus only user-expanded `Ready` files and stopped populating
edges entirely.

## Full path from `A` to the generic rejection

1. **Dispatch.** `A` maps to `Action::AiRefresh`
   (`crates/codescope-tui/src/action.rs:183-186`), the runtime forwards it
   (`crates/codescope-tui/src/run.rs:199-201`), and the dispatcher calls `spawn_ai`
   (`crates/codescope/src/dispatcher.rs:388-390`).

2. **Digest coverage.** `spawn_ai` walks the changeset and adds symbols and diagnostics
   only from `FileSemanticState::Ready` entries
   (`crates/codescope/src/dispatcher.rs:604-622`). It passes an empty
   `ImpactGraph` with `Completeness::Unknown` and the correct “relations not queried”
   note (`:623-633`). The digest builder caps hunks at 40
   (`crates/codescope-analysis/src/digest.rs:20-27,291-335`) and builds relations from
   the supplied graph (`:338-367`). Rendering turns symbols, hunks, and relation
   endpoints into human text (`:388-479`). `spawn_ai` appends the non-Ready coverage note
   at `crates/codescope/src/dispatcher.rs:635-660`.

   With no expanded files, the useful outbound tiers are therefore `hunks` (at most 40)
   and the shallow repo sketch. This is enough for a `focused_diff` or a file-backed
   `impact_summary`, but not for a factual symbol/call/type view.

3. **Validation facts.** `SnapshotFacts::from_lazy` inserts all changed file identities
   and hunk counts, but inserts symbols only from each `Ready` result's `changed` list and
   never inserts an edge (`crates/codescope/src/dispatcher.rs:1667-1704`). The boolean
   `edge_exists` can therefore return only `false` (`:1714-1731`). Even a `Ready` file does
   not contribute all symbols from its cached worktree tree; only mapped changed symbols
   enter this view.

4. **Model request and false tool affordance.** The dispatcher captures that fact view
   and calls the service with `NoToolExecutor`
   (`crates/codescope/src/dispatcher.rs:661-669`). The service nevertheless installs all
   nine definitions unconditionally (`crates/codescope-ai/src/service.rs:147-154`;
   definitions at `crates/codescope-ai/src/tools.rs:46-165`). `NoToolExecutor` reports
   `"no fact store wired"` for every call (`crates/codescope-ai/src/tools.rs:211-227`).
   Its comment says the model is told tools are unavailable, but that happens only after
   the model spends a call on one; the initial request says the opposite.

   The system prompt also offers all eight form kinds and says entities and edges may be
   copied from the digest or a tool result
   (`crates/codescope-ai/src/service.rs:292-313`). The plan tool schema repeats all eight
   kinds and all six edge kinds (`crates/codescope-ai/src/plan.rs:44-129`). Thus the
   request steers the model toward outputs the captured fact view cannot accept.

5. **Parse, then local validation.** A submitted plan is parsed successfully and sent to
   `validate` (`crates/codescope-ai/src/service.rs:165-170`). Entity validation treats
   `resolve_symbol == None` as nonexistence
   (`crates/codescope-ai/src/validator.rs:139-188`), although on the cold path it means
   “never queried.” The consequences are deterministic:

   - a tree rejects an invalid root and rejects when more than 20% of nodes are invalid
     (`crates/codescope-ai/src/validator.rs:337-349`);
   - a flow/sequence rejects any invalid endpoint before checking edges (`:474-498`),
     then rejects a calls/imports/implements/contains edge when the boolean fact lookup is
     false (`:498-523`);
   - a list drops invalid bullets and rejects when none remain (`:555-599`); and
   - non-flow verifiable edges are dropped on the same boolean lookup (`:602-653`).

   When every form is removed, validation adds only the generic final note
   (`crates/codescope-ai/src/validator.rs:106-135`).

6. **The useful reasons are discarded.** The form/node reasons are present in
   `ValidationReport.dropped`, but the Rejected branch constructs the failure solely from
   `report.notes` (`crates/codescope-ai/src/service.rs:170-179`). The dispatcher then adds
   the retry/model/fallback suffix (`crates/codescope/src/dispatcher.rs:1006-1039`). This
   produces the observed message while hiding reasons such as “root symbol X does not
   resolve” or “edge was not in the impact graph.”

## Why `0c6e7da` caused the regression

At `0c6e7da^`, `spawn_ai` required the eager `AnalysisSnapshot`, rendered
`analysis.digest()`, and built facts with `SnapshotFacts::new`. That constructor copied
all eager changed symbols and graph edges. Commit `0c6e7da` deliberately stopped the
eager pass and changed this to `from_lazy`; the new constructor left `edges` initialized
but never populated it. The performance redesign is valid, but the AI request contract
was not narrowed to match the new evidence boundary.

There were two latent issues before the redesign that are now exposed more often:

- the old fact builder added unchanged graph-neighbor edges but did not add those
  neighbors to its symbol-resolution map, so a relation endpoint outside the changed
  symbol set could still fail before its known edge was checked; and
- `PlanEdgeKind::Imports` has no `RelationKind::Imports` source in
  `crates/codescope-core/src/relation.rs:15-37`. The current shallow graph builds only
  implementation edges and deliberately omits calls
  (`crates/codescope-analysis/src/graph.rs:32-93`). `contains` and imports are therefore
  advertised much more broadly than the producer can prove.

## Assessment of the proposed directions

### 1. Request-scoped facts from lazy results plus selected relations

**Useful, but not sufficient alone.** It is cheap, epoch-local, and reuses LSP work that
has already happened. Known callers must become `caller -> selected` calls edges and
known callees `selected -> callee` calls edges. A returned edge is positive evidence even
when the query was partial. A missing edge is negative evidence only for the exact
queried direction/anchor when that query was `Complete`.

The current cache cannot be consumed authoritatively as-is. `SelectedRelations` holds
`RelationRows` (`crates/codescope/src/dispatcher.rs:129-136`), and `relations_for`
converts each `SymbolRef` to an `ImpactRow` label while discarding file, kind, notes, and
all identity except the name (`:1734-1759`). Refactor the cache to retain raw
`Evidence<Vec<SymbolRef>>` (or better, exact `EntityRef` relation facts), and derive UI
rows at publish time. Also note that `SymbolRef` has no range
(`crates/codescope-core/src/semantic.rs:412-425`); the fact API needs to represent a
confirmed symbol whose extent is not known, or the result must be enriched from an
outline.

This direction does nothing for the dominant zero-expanded-file path because there is no
selected symbol or loaded relation to reuse.

### 2. A real bounded read-only `ToolExecutor`

**Best semantic-quality follow-up, but too large to be the first correctness fix.** A
real executor can turn the cold path into a useful semantic plan without touching all 83
files: the model can inspect one hunk/file, request one outline, then query one relation.
The existing service already has an eight-call budget.

It must share a request-scoped, mutable fact catalog with validation. A tool result that
the model sees must add the same exact entity/edge to the later `FactView`; otherwise the
model can faithfully echo a tool result and still fail local validation. The executor
also needs an epoch snapshot, repo-root sandboxing, result caps, a wall-clock/LSP-query
budget, and no lock held across an LSP await.

Do not implement all nine names merely to match the current table. Advertise only working
capabilities. In particular, the current analysis/LSP abstraction has no workspace-symbol
search method, caller/callee tool arguments identify only a naked `symbol` rather than an
unambiguous file+symbol entity, depth two needs more positions/outlines, and some
implementation responses currently use range-derived placeholder names. Start with a
small honest subset such as `get_hunk`, `get_file_outline`, `get_symbol`, and depth-one
caller/callee queries. Let tool definitions grow with implementation and LSP feature
availability.

### 3. Dynamic capability honesty

**This is the strongest immediate fix.** Form kinds, edge kinds, entity types, and tool
definitions should be derived from facts visible in this request, not from the maximum
product schema. On the cold path, advertise `focused_diff` plus a file-backed `impact_summary`;
advertise no semantic relationship forms and no read tools when using `NoToolExecutor`.
After exact symbols or edges become visible, unlock only the forms they can support.

This must be enforced in both the prompt/plan-tool schema and the validator. Prompt text
alone is not a boundary, and a provider may ignore a JSON-schema enum. A plan using a
capability not offered for that request should fail with “not available: relation not
queried,” not with “edge does not exist.”

Tradeoff: a cold AI answer is less semantic. That is the only honest result without doing
more LSP work. If richer cold answers are a product requirement, add the bounded executor
above; do not silently return to eager analysis.

### 4. Structured, ready-to-echo entity JSON

**Required for reliability, independent of the fact-volume choice.** Emit JSONL or a
small JSON array such as:

```json
{"fact":"file","entity":{"file":"src/lib.rs"}}
{"fact":"hunk","entity":{"file":"src/lib.rs","symbol":"hunk:0"}}
{"fact":"symbol","entity":{"file":"src/lib.rs","symbol":"Parser::parse"}}
```

Generate it with `serde_json`, and tell the model to copy the `entity` object exactly. A
future plan-version bump could replace object copying with an opaque `fact_id`, which is
even safer and smaller.

The current text is not ready to echo. A changed symbol is rendered as
`path:symbol` (`crates/codescope-analysis/src/digest.rs:408-437`), a hunk as
`path#hN` (`:452-463`) even though the plan must transform it to `symbol: "hunk:N"`, and
relations use producer-formatted string node ids (`:466-470`; model at `:100-113`). Rust
`::`, Go receiver names, colons in legal paths, punctuation, case, escaping, and Unicode
all make splitting and reconstruction error-prone. The prompt's “copy verbatim” rule is
therefore not literally possible for hunks and is ambiguous for symbols/relations.

### 5. Tri-state edge evidence

**Required for correctness.** Replace the boolean with at least
`Present | Absent | Unknown`. `Absent` must mean a complete query covered this exact
candidate and did not return it. An empty `ImpactGraph` with `Completeness::Unknown` means
all edge lookups are `Unknown`, not `Absent`. For a partial query, returned edges are
`Present`; non-returned edges are `Unknown`.

Apply the same distinction to symbols because it is the dominant cold-path failure:
`Found`, `AbsentAfterCompleteOutline`, and `Unqueried/Unavailable`. The engine currently
unwraps `Evidence<SymbolTree>` into the value and retains only free-form notes
(`crates/codescope-analysis/src/engine.rs:310-330`), so a complete versus partial outline
cannot currently support authoritative negative lookup. Preserve typed query coverage.

I do **not** recommend retaining an Unknown edge as an ordinary relationship merely by
adding a `ValidationReport.notes` entry. The dispatcher renders `plan_rows` and discards
report notes on success (`crates/codescope/src/dispatcher.rs:1016-1029`), so the user
would see an unverified edge as fact. With current render types, Unknown should fail as
“not queried/cannot validate,” while capability honesty keeps the normal model path away
from it. If graceful degradation is desired, add an explicit validated-edge state and a
visible `?`/dashed rendering; only then may Unknown remain. `Absent` after a complete
query must retain the current form-killing behavior.

### 6. Rejection diagnostics

**Do this regardless of the primary design.** It is small and immediately turns the
current generic symptom into an actionable report. See the concrete fix below.

## Primary recommendation: a capability-honest `AiFactContract`

Build one immutable, request-scoped `AiFactContract` from the changeset, existing lazy
cache, and any raw selected-relation evidence. That contract is the single source for
(1) tri-state validation lookup, (2) the exact structured entity catalog placed in the
prompt, and (3) the allowed form/edge/tool capabilities placed in the prompt and dynamic
plan schema. On a zero-Ready request it offers only exact file/hunk entities and
`focused_diff`/file-level `impact_summary`, so a scripted model can produce a valid plan
without any LSP query. Known selected edges can unlock call forms. No missing fact is
called nonexistent unless a complete scoped query proves absence, and no eager analysis
is started. A bounded real executor can later extend the same contract during a request,
but it is not required to repair the cold-path contract now.

### Concrete type and API sketch

In `crates/codescope-ai/src/validator.rs`:

```rust
pub enum FactLookup<T> {
    Present(T),
    Absent,   // a complete, matching query proves no result
    Unknown,  // unqueried, failed, unsupported, or partial non-result
}

pub struct SymbolFact {
    pub extent: Option<LineRange>, // LSP may confirm identity without an extent
}

pub trait FactView: Sync {
    fn file(&self, file: &FileId) -> FactLookup<()>;
    fn symbol(&self, file: &FileId, name: &str) -> FactLookup<SymbolFact>;
    fn edge(&self, from: &EntityRef, to: &EntityRef,
            kind: PlanEdgeKind) -> FactLookup<()>;
    fn hunk(&self, file: &FileId, index: u32) -> FactLookup<()>;
}
```

Add `PlanCapabilities` (in `codescope-ai`, or in core if the TUI needs it) with allowed
form kinds, edge kinds, and entity classes. Pass it to `validate`; reject a form not
advertised for that request before form sanitization. Require fact-backed entities for
data-bearing nodes; an omitted entity must not be usable as a relationship endpoint.
Today `entity: None` is accepted as presentational
(`crates/codescope-ai/src/validator.rs:163-166`) and a flow edge with presentational
endpoints is retained with only a note (`:503-516`), which is a separate bypass of the
fact boundary. Also validate the semantic child links of `call_tree` and
`type_impl_tree`: `sanitize_tree` currently treats `children` only as structure and never
checks that the implied call/implementation exists (`:311-449`). Likewise, do not offer
unverified reads/writes edges merely because the schema contains them
(`:213-223,517-521`).

In a new small runtime module such as `crates/codescope/src/ai_facts.rs` (rather than
adding more to the 3k-line dispatcher), define:

```rust
struct AiFactContract {
    facts: RequestFacts,
    visible_entities: Vec<PromptEntity>,
    capabilities: PlanCapabilities,
}

struct RequestFacts {
    files: HashSet<FileId>,
    hunks: HashSet<(FileId, u32)>,
    symbols: HashMap<(FileId, String), SymbolFact>,
    symbol_coverage: HashMap<FileId, QueryCoverage>,
    edges: HashSet<EdgeKey>,
    edge_queries: Vec<EdgeQueryCoverage>,
}

enum QueryCoverage {
    NotQueried,
    Queried(Completeness),
}
```

`EdgeQueryCoverage` must describe kind, anchor, direction, and completeness; a global
“graph complete” flag is not precise enough for one selected symbol's incoming/outgoing
query. Construct the contract without I/O from `ChangeSet`, `FileSemanticState::Ready`,
and raw `SelectedRelations`. Clone it into the spawned request exactly as current facts
are cloned, preserving the existing epoch and generation gates.

In `crates/codescope-analysis/src/engine.rs`, retain document-symbol query completeness
instead of reducing `Evidence<SymbolTree>` to `SymbolTree` plus strings. In
`crates/codescope-analysis/src/digest.rs`, carry or render exact `EntityRef` JSON for each
visible symbol/hunk/relation endpoint. Apply the existing token/hunk/symbol caps before
computing capabilities, so every enabled capability has at least one prompt-visible fact.

Superseded implementation note: the runtime now always exposes the shared incremental diagram
editor and finish operation. It no longer appends or accepts a whole-plan submission tool.
Capabilities should narrow the research and edit guidance passed to that single protocol.

In `crates/codescope-ai/src/tools.rs`, add an availability method, for example:

```rust
pub trait ToolExecutor: Send + Sync {
    fn definitions(&self) -> Vec<ToolDef>;
    fn execute<'a>(&'a self, name: &'a str, args: &'a Value)
        -> BoxFuture<'a, Result<String, ToolExecError>>;
}
```

`NoToolExecutor::definitions()` returns empty. The service must authorize against the
request's advertised names, not merely the global nine-name list. A later real executor
can share an `Arc<RwLock<RequestFacts>>` with validation and insert exact facts after each
bounded call.

### Tradeoffs of the recommendation

- **Pros:** fixes the exact cold path; keeps deterministic validation authoritative; makes
  absence semantics honest; no eager LSP work; prompt and validator cannot drift; reuses
  selected work when present; works with scripted providers.
- **Cons:** dynamic JSON schema and tri-state lookup touch several crates; cold plans are
  deliberately limited; preserving scoped completeness is more complex than a boolean;
  structured entities consume some tokens. A mutable tool-enabled contract adds locking,
  cancellation, and query-budget work and should be a follow-up.

## Diagnostics fix

At `crates/codescope-ai/src/service.rs:170-175`, replace `report.notes.join("; ")` with a
bounded helper that prioritizes concrete form-level `report.dropped` entries, then node
entries, then notes only when no dropped reason exists. For example:

```text
plan rejected: form 0 (RelationshipFlow): endpoint n1 invalid: symbol Parser::parse
was not queried in src/lib.rs (+2 more)
```

Requirements for the helper:

- include at most two reasons and an omitted-count suffix;
- collapse whitespace and strip all terminal/control characters;
- scrub secret-looking content with the existing secret scrubber;
- truncate each reason and the total by Unicode scalar count, not byte slicing; and
- preserve the full typed `ValidationReport` for debug logging/panes, but never put an
  unbounded model-controlled subject/reason on the status line.

The generic “no renderable forms remain” note can follow the concrete reason only if room
remains. The dispatcher already appends recovery guidance, so the service summary should
spend its budget on the cause.

## Required tests (all offline/scripted)

1. **Cold 83-file end-to-end regression.** In a dispatcher/service test, build an
   83-file changeset, leave `file_semantics` empty, and use `ScriptedProvider` to return a
   `focused_diff` plan that echoes a structured visible hunk entity. Assert
   `AiOutcome::Plan`, a renderable row, and zero fake `document_symbols`, incoming-call,
   outgoing-call, and implementation queries. This proves no eager analysis returned.

2. **Cold wire contract.** Inspect the fake provider's captured first request. Assert the
   plan schema contains only the cold-safe forms, the user message contains exact
   file/hunk `entity` JSON, no semantic relationship form/edge is offered, and the nine
   unavailable read tools are absent when `NoToolExecutor` is used.

3. **Capability enforcement.** Have a scripted provider ignore the narrowed schema and
   submit `relationship_flow` on the cold contract. Assert deterministic rejection says
   “relation not queried/not available for this request,” not “edge does not exist.”

4. **Tri-state edge matrix.** Validator unit tests must cover: returned edge under a
   partial query is `Present`; a different non-returned edge under that partial query is
   `Unknown`; non-returned under a matching `Complete` query is `Absent` and rejects; an
   unrelated/unqueried edge is `Unknown`; and empty `Unknown` evidence never produces an
   absence claim.

5. **Tri-state symbol matrix.** An unloaded/failed/partial file gives `Unknown` for an
   unseen symbol; a returned symbol is `Present`; only an absent name under a preserved
   complete outline is `Absent`. The rejection text must distinguish unknown from proven
   absent.

6. **Selected relation ingestion.** Store raw fake caller/callee evidence and build a
   request contract. Assert caller and callee edge directions, that known endpoints become
   resolvable entities, that complete-empty coverage is scoped only to its queried anchor,
   and that appropriate call capability becomes available only when prompt-visible edge
   evidence exists.

7. **Entity transcription regression.** Use paths/symbols containing `:`, Rust `::`,
   quotes/backslashes, and Unicode. Capture the prompt, have the scripted model echo the
   JSON object, and assert exact `EntityRef` equality and successful validation. Include
   the hunk `path#h0` to `{"file": path, "symbol": "hunk:0"}` case without requiring the
   model to transform text.

8. **Concrete bounded diagnostics.** A scripted hallucinated plan must return a failure
   containing its concrete dropped-form reason. Separate unit cases inject newlines,
   escape/control characters, a secret-looking token, very long Unicode text, and many
   drops; assert sanitization, secret scrubbing, item/character caps, and omitted count.

9. **If the real executor is included now:** use a fake executor/LSP source whose outline
   result was *not* in the initial facts, then submit a scripted plan that cites the tool's
   exact returned entity. Assert the shared request facts make it validate. Also assert the
   call, result-size, unique-file, and wall-clock budgets and repo path sandbox. The
   existing tool-loop test does not prove this because its `FixtureFacts` already accepts
   the plan independently of tool results.

No live API test is necessary or appropriate for any of these invariants.
