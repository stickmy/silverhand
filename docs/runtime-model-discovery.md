# Runtime model suggestions

The create-agent dialog and runtime configuration editor discover model IDs on the
selected computer. The model field remains a free-text combobox: discovered IDs
are suggestions, never an allowlist, and a refresh never changes the draft or a
saved model. Runtime defaults are unchanged.

The owner-only `GET /api/hosts/{id}/runtime-catalog` accepts optional
`models_for=<runtime>`. It forwards that option through the existing
`host_runtime_catalog` daemon command. Ordinary installation/version inventory
requests do not discover models. A discovery response adds:

```json
{
  "model_discovery": {
    "runtime": "pi",
    "status": "ok",
    "models": ["provider/model-id"],
    "detected_at": 1700000000000,
    "cached": false
  }
}
```

`status` is `ok`, `unsupported`, or `error`; non-success responses have no models.
An empty successful list is distinct from failure. The server checks the requested
runtime and binds the result to the existing host connection custody. It forwards
`models_for` only to a daemon advertising `runtime.model_discovery.v1` and answers
`unsupported` itself otherwise; a malformed discovery is dropped without failing the
inventory. No new permissions, persisted model table, or runtime library dependency
is introduced.

## Native adapters

| Runtime | Discovery |
| --- | --- |
| Claude | Print-mode stream-json `control_request/list_models`; keep aliases and resolved IDs, exclude disabled/default entries. Disable hooks, configured MCP, and session persistence. |
| Codex | `app-server`: initialize, then paginate `model/list`; exclude hidden entries. No thread or turn starts. |
| Pi | In-memory RPC `get_available_models`; preserve `provider/id`. Extensions and skills are disabled for this metadata probe. |
| OpenCode | `models`; version 2+ adds `--standalone` to avoid its shared background service. Unknown versions decline discovery. |
| Grok | `agent --no-leader stdio`: ACP initialize metadata `_meta.modelState.availableModels`; no session starts. |
| Qoder | No discovery. Keep Auto plus Ultimate / Performance / Efficient / Lite, with the existing default. |

Each probe uses the installed executable resolver, a private temporary working
directory, the driver's environment rules, and the computer's configured runtime
proxy. It sends no prompt. A 15-second deadline and a 2 MiB output limit bound the
probe; its process group is terminated and temporary directory removed. Raw
runtime output and credentials are not sent to the renderer.

Successful results have a 60-second in-memory daemon cache scoped to runtime,
executable/version, and effective environment. Concurrent identical requests
share one probe. Force refresh bypasses completed cache entries; failures are not
cached by the daemon. Renderer queries use a separate host/runtime key and expose
a refresh icon in the Model label row that spins while loading and turns red on failure. Authentication changes that
do not change the environment become visible on refresh or cache expiry.

These are computer-level suggestions, not a project-specific model catalog:
project configuration and Pi extensions can add models absent from the list.
Users can enter those IDs directly. The list also does not guarantee inference
quota or access; the runtime remains authoritative when it runs the model.

## Native reasoning effort

Claude `supportedEffortLevels` and Codex `supportedReasoningEfforts` feed optional
`reasoning_by_model: { [modelId]: { supported_efforts, default_effort? } }` in the
same discovery response. Claude aliases and resolved IDs carry the same metadata.
Codex reports `default_model` from `isDefault`; Claude reports the driver default
`opus`. Only Codex currently reports an effort default. Missing/malformed effort metadata means unknown; an explicit
empty list means no advertised effort options. Tokens have the existing bounded
lowercase format. The Rust boundary validates metadata shape and model references,
not a static list of model capabilities.

The runtime configuration editor uses that model's discovered effort options,
falling back to the runtime's static effort vocabulary (`reasoningEffortOptions`)
when metadata is absent. It retains a saved/draft effort even if the refreshed list
omits it. The default choice remains null; the reported default is a label, never
an automatic persisted selection. `unsupported` shows no refresh control and uses the
static catalog, as before discovery existed.

For every runtime with a Reasoning control, create/switch/PATCH enforce token shape
only; a model change never clears or rejects the effort. The CLI owns actual
model/effort compatibility: an unsupported value may fail startup or the turn
through the existing runtime error path, or the CLI may fall back to its default
(Claude 2.1.300 warns and falls back). Silverhand keeps that vendor behavior.
Claude now passes explicit effort via `--effort`.

## Codex service tier

Codex `serviceTiers` feed optional `service_tiers_by_model: { [modelId]: [{ id, name }] }`
(non-default tiers only). The agent stores `service_tier` (null = Codex default,
`default`, `priority` = Fast, `ultrafast`, ...); writes check token shape only. The
driver passes it as `serviceTier` on `thread/start`/`thread/resume`; Codex falls back
to Standard when the model or CLI lacks the tier. The Speed control lists Standard
plus the model's discovered tiers, else the static `priority`. Only Codex ≥0.169
reports `ultrafast` (currently `gpt-6.2-sol`).

The old `fast_mode` flag never selected a tier, so nothing maps onto it. Only
strict old decoders are kept working: a daemon without `runtime.service_tier.v1` is
sent `fast_mode: null`, and the agent DTO carries `fast_mode: null`. Writes no longer
accept `fast_mode`.

## Compatibility and verification

An old server may omit `model_discovery`; an old daemon is never sent the new
parameter. Both leave manual input usable. Dynamic discovery requires an updated
server and daemon; existing clients keep using ordinary catalog responses.
Explicit Claude effort additionally requires the updated daemon: old drivers
ignored that field. Upgrade the daemon before configuring it; no deployment or
local executable replacement is performed by this change.
Vendor metadata protocols can change. Unsupported or failing probes must not
prevent creating or editing an agent with a manually entered model ID.

`packages/host/tests/runtime-models.test.ts` covers adapters, pagination, deadlines,
output limits and caching. Dispatcher, shared schema, Rust transport/HTTP and UI
query tests cover the cross-layer contract. Native smoke probes send metadata
requests only; do not send an inference prompt to test catalog discovery.

Reference mechanisms were examined, not imported:

- [OAR Claude protocol reference](https://github.com/botiverse/oar/blob/main/packages/oar/src/runtimes/claude/list-models.ts) — no OAR dependency is used.
- [Claude CLI flags](https://code.claude.com/docs/en/cli-reference)
- [OpenCode CLI](https://opencode.ai/docs/cli/)
- [Grok custom models](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md)
