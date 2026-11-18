# Rust request validation convergence

## Decision

Use Serde types as the default structural validator for Rust HTTP requests. A
handler should receive a typed request, not `Bytes` or `serde_json::Value`, unless
the payload is intentionally dynamic or must be streamed.

The target stack has three explicit layers:

1. **Transport admission (`backend-server`)** — Axum's native content-type, body,
   JSON syntax, and Serde data rejections.
2. **Request semantics (`backend-server`)** — patch presence algebra and small
   request-local refinements such as “at least one field”.
3. **Domain admission (`backend-core`)** — reusable names/identities, cross-field
   legality, runtime catalog/custody, and rules that depend on current state.

Use the first public-agent PATCH conversion to test the ecosystem integration
`axum-valid + garde` alongside Serde. Adopt it only if the proof materially removes
stateless constraint boilerplate without changing Axum's native JSON rejections or
pulling stateful domain rules into the HTTP layer.

## Current inventory

The converged Wave 3 tree contains three different request styles:

- **Already typed:** several host-management inputs and daemon protocol frames use
  `Deserialize` structs/enums. These demonstrate the desired structural boundary,
  although a few fields still deserialize to `Value` and classify afterward.
- **Handwritten JSON schema replicas:** representative high-cost sites include
  `host_agent/agent_patch.rs`, `human_auth.rs`, `workspace_metadata_write.rs`,
  `host_runtime/{claim,delete}.rs`, `access/wire.rs`, messaging send admission,
  reaction/notification/attention/runtime-control inputs, and parts of channel
  mutation. They manually repeat root-object checks, field type matches, enum/token
  checks, missing/null/value handling, Zod issue construction, and refine order.
- **Streaming multipart:** user avatar, public agent avatar, internal profile/avatar,
  and attachments correctly require streaming/bounded reads, but currently repeat
  field iteration, draining, first-slot selection, MIME normalization, and cap
  handling.

No `garde`, `validator`, `axum-valid`, `serde_with`, or equivalent validation
dependency is currently present. This is useful: the first phase can converge on
native Axum + Serde without a dependency migration.

`serde_json::Value` is not itself a problem. It remains appropriate for genuinely
dynamic JSON, opaque payloads intentionally persisted verbatim, protocol envelopes
whose payload is open by contract, response assembly, and tests. The target is to
remove it from closed request schemas.

## Target primitives

### 1. Framework-native typed JSON extraction

Use Axum's `Json<T>` directly. Do not add a project-wide wrapper or remap its
standards-aligned status codes:

| Failure | Boundary result |
| --- | --- |
| Invalid JSON syntax | Axum `400 Bad Request` |
| Serde data failure (wrong type, missing required field, illegal enum, duplicate closed field) | Axum `422 Unprocessable Entity` |
| Missing/wrong JSON content type | Axum `415 Unsupported Media Type` |
| Body/envelope over the production bound | `413 Payload Too Large` |
| Body stream/read failure | Axum's typed transport rejection; do not pretend it is a domain validation error |

Treat Axum's rejection body as framework diagnostics, not a product schema to copy
elsewhere. A route may override a native rejection only when an existing public
consumer demonstrably depends on a different stable envelope/status. Such an
exception stays local and typed; it does not justify a global mapper or handwritten
parsing.

Body-first is the default. Authentication/tenant isolation may still be layers in
front of the extractor. Any other pre-body exception must name the actual consumer,
attack, or bounded resource it protects and carry a discriminator that fails if the
ordering is removed.

### 2. Native Serde structure

Use derived request structs and enums for:

- root object and required fields;
- primitive field types;
- closed string vocabularies (`#[serde(rename_all = "snake_case")] enum ...`);
- tagged or untagged unions;
- objects with Serde's default unknown-field behavior (unknown keys are ignored;
  reject a specific key only when its presence must never be accepted).

Do not reproduce Zod issue arrays, declaration-order error accumulation, or
simultaneous-invalid-field precedence without an evidenced public consumer.

### 3. Shared patch presence algebra

Define one server-side generic request type:

```rust
enum PatchField<T> {
    Missing,
    Null,
    Value(T),
}
```

`Default` produces `Missing`; its custom `Deserialize` maps an explicitly present
`null` to `Null` and any other present value through `T`. A request struct uses
`#[serde(default)]` on each patch field. This replaces route-local `Tri`, nested
`Option` ambiguity, raw-map `contains_key`, and duplicated missing/null/value
matches.

Also provide a small “must be absent” field helper for schemas such as
`runtime: z.never().optional()`: a missing field defaults successfully and any
present value, including `null`, rejects during deserialization.

These are wire vocabulary only. They convert into capability-specific core edits;
they do not become persistence types.

### 4. Optional semantic validation wrapper

The preferred ecosystem candidate is `axum-valid` with only its `garde`, `basic`,
and `422` features, plus `garde` derive. Its outer `Garde<Json<T>>` extractor first
runs Axum's inner `Json<T>` extractor and preserves that rejection; only a
successfully deserialized value reaches stateless semantic validation. Configure
validation failures as `422`, not the crate's default `400`.

Use Garde for repeated, stateless field constraints: numeric ranges, collection
bounds, nested validation, and shared custom functions. Do not publish Garde's
error representation as a stable product schema.

Alternatives are deliberately not selected:

- `validator` is mature and also supported by `axum-valid`, but Garde's custom and
  contextual validation model better fits the current Rust types.
- `validify` combines mutation/normalization with validation; that is too implicit
  for this boundary because normalization and domain admission must stay visible.
- `serde_with` is useful for serialization adapters such as double-option, but it
  is not a validation layer; prefer a named `PatchField<T>` over nested options.

### 5. Newtypes and domain admission

Prefer existing domain newtypes (`DisplayName`, IDs, email/name authorities) via
`TryFrom<String>` or a narrow custom deserializer when the rule is context-free.
Use four repeatable implementation shapes rather than per-route validation
templates:

1. **Context-free scalar rule:** one validated newtype with `TryFrom`; Serde
   delegates to it. Examples are IDs, email/name authorities, `JsSafeInteger`, and
   a bounded UTF-16 string when multiple fields truly share the same semantics.
2. **Patch presence:** `PatchField<T>` supplies missing/null/value once for every
   route.
3. **Request-local combination:** `TryFrom<RawRequest> for AdmittedRequest` owns
   “at least one”, mutual exclusion, and other pure cross-field rules. The JSON
   extractor remains generic and the handler sees no raw map.
4. **State-dependent decision:** a core `admit(raw, &context)` function owns rules
   that read a catalog/current row/custody; write-sensitive decisions are replayed
   under the unit-of-work lock.

Do not force these rules into a generic validation derive:

- ECMAScript trim and UTF-16 length where the product actually uses them;
- “at least one known field” and other request-local refinements;
- runtime × model × reasoning-effort legality;
- rules depending on a catalog, current row, ownership, custody, or a write lock.

Closed single-field vocabularies should be enums. Open token grammars should be
newtypes. Dynamic cross-field legality stays a core decision, not a combinatorial
wire enum.

Garde custom rules may delegate to shared ECMAScript/UTF-16 scalar authorities. It
must not absorb stateful core admission, define public error text, or replace
`PatchField`/typed enums with a stringly rule registry.

### 6. Multipart stays streamed

Do not force multipart through a JSON-style derive or aggregate the envelope.
Extract shared, route-neutral streaming plumbing only:

- bounded field read (`cap + 1`) and chunk drain;
- declared MIME normalization;
- typed distinction between text and file slots;
- transport/multipart failure classification;
- optional reusable first-slot/duplicate policy when two real routes share it.

Each route family still declares its own fields and request algebra. This avoids
building a generic multipart schema engine while removing the currently copied
memory-safety and draining logic.

## Migration sequence

Run this only after the Wave 3 worktrees have converged, so validator churn does
not overlap route migration or obscure its acceptance evidence.

### Phase 0 — foundation

1. Add the shared request vocabulary `PatchField` and `MustBeAbsent` in a small
   crate-private request-admission module; ordinary endpoints use `Json<T>`.
2. Prove Axum's native status matrix through a real router: syntax, wrong type,
   required field, enum, unknown-field strict/strip, missing/null/value, 413, and
   wrong content type.
3. Spike `axum-valid = 0.25` + `garde = 0.23` on public-agent PATCH with validation
   failures configured as `422`; prove the inner Axum 400/415/413 rejections are
   unchanged. Retain the dependencies only if the resulting production code is
   materially smaller than typed Serde plus `TryFrom` alone.
4. Add an ownership/inventory gate: JSON route modules may not add new
   `parse_*_body(&Bytes)` or closed-schema `serde_json::Value` parsers without an
   explicit exception entry.

### Phase 1 — simple, high-return conversions

Convert routes where Serde replaces almost all code:

- public agent PATCH;
- signup/login/profile PATCH;
- host delete consent and empty-object inputs;
- host update/proxy fields that currently deserialize to `Value` only to distinguish
  null/string;
- small reaction, attention, notification, and runtime-control request objects.

For each route, delete obsolete Zod issue builders and ordering tests. Keep one
accepted request, one representative structural rejection, and any real domain
boundary discriminator.

### Phase 2 — unions and patch algebra

Convert workspace metadata, workspace member selection, channel mutations, and
message-send closed portions using `PatchField`, typed enums/unions, and explicit
`TryFrom<RawRequest>` refinements. Preserve unusual fallback semantics only if a
current consumer is demonstrated; otherwise adopt the simpler typed contract.

Message content that is intentionally arbitrary JSON remains `Value` inside an
otherwise typed envelope.

### Phase 3 — multipart plumbing

Converge avatar/profile/attachment streaming helpers. Keep domain caps and error
codes where they are product contracts; remove copied chunk/drain/MIME machinery.

### Phase 4 — dependency audit

Audit the `axum-valid + garde` usage after Phases 1–3. Keep it only where it
materially removes repeated declarative validation while preserving:

- custom ECMAScript/UTF-16 semantics explicit;
- domain decisions in core;
- framework-native rejection statuses by default;
- no public exposure of library error text/path internals.

If most usages have become custom cross-field or domain-dependent callbacks, remove
the validation dependencies. Native Serde plus typed `TryFrom` is then the simpler
final model.

## Acceptance gates

- Closed JSON request schemas reach handlers as typed values; no new handler-level
  `Bytes -> Value -> Map` parser.
- JSON syntax/data/content-type/limit failures retain Axum's native status classes;
  no global custom mapper exists.
- Wrong primitive type, missing required field, and illegal enum all stop in the
  extractor without executing the handler.
- Garde semantic failures are `422`; wrapping a request with Garde does not alter
  the inner Axum syntax/content-type/body-limit status.
- `PatchField` has resident missing/null/value tests, including invalid inner values.
- Unknown-field behavior is explicit per request type (strict or strip), never an
  accidental parser property.
- 413 remains distinct from JSON syntax/data rejection; largest admitted payload and cap+1 are
  exercised through the composed router.
- No refusal-order test survives on a generic “security” or “parity” claim; every
  exception identifies its real value and mutation-killing discriminator.
- Dynamic JSON exceptions are listed and justified; the inventory gate ignores
  response construction, tests, and protocol/open-payload types.
- Full relevant Rust tests, cold clippy `-D warnings`, fmt, and diff-check pass on
  the converged tree.

## First proof point and falsifier

Use public agent PATCH as the first conversion after convergence. It currently
contains the representative manual machinery: root/object parsing, forbidden
field, five field types, missing/null/value algebra, closed/open token validation,
and at-least-one refinement.

The proof succeeds if the typed version deletes the raw `Value` parser and its
field-by-field type branches while preserving the accepted update algebra and core
tuning legality. It fails if achieving parity still requires endpoint-local
extractor ordering, a custom global rejection wrapper, treating framework error
text as product API, or more custom code than the parser it replaces. That
falsifier decides whether to adjust the shared primitives before converting the
wider tree.
