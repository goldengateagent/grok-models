# Plan: Grok Build provider model sync

Build this in the **current project directory**. The script (`grok-models.py`) and its
data files live here. The script writes `models.toml` (in this same directory), which
Grok Build reads as the managed model config.

## Goal

Maintain custom `[model.*]` tables for one or more providers. The user adds providers;
the script pulls each provider's models and metadata **live from
`https://models.dev/api.json`**; the user can enable/disable providers and models;
enabled models are written into `models.toml`.

No separate per-provider model-list URL, no `models.json`, and no `models-cache.json`.
`models.dev/api.json` is the single source of truth for everything (base URL, env key,
context window, reasoning).

## Files

| File | Role |
|---|---|
| `grok-models.py` | CLI (stdlib only, no third-party packages) |
| `providers.json` | User-managed list of providers + their model enabled flags |
| `models.toml` | Generated Grok Build `[model.*]` config (output) |
| `models.dev.json` | Downloaded snapshot of `models.dev/api.json`, kept as a reference only |

### `providers.json`

Minimal: a list of providers. Each provider stores its `id`, a human-readable `name`
(resolved from models.dev at add time, for reference), an `enabled` flag, and a `models`
map of model-id → `{enabled}`. The `api`/base URL, `env_key`, and model metadata are
fetched live from `models.dev/api.json` at sync time and are **not** duplicated here.

```json
{
  "providers": [
    {
      "id": "opencode-go",
      "name": "OpenCode Go",
      "enabled": true,
      "models": {
        "hy3": { "enabled": true },
        "kimi-k3": { "enabled": false }
      }
    }
  ]
}
```

- `models` is keyed by the model's full id from models.dev (e.g. `anthropic/claude-opus-4.7`
  or `claude-opus-4-7`, depending on the provider).
- `enabled` at the provider level gates the whole provider; `enabled` per model gates
  that single model. A table is written only when **both** are true.
- Adding a provider populates `models` with every live model, all `enabled: false` by
  default (the user enables the ones they want).

### `models.toml` (output)

Generated in the script's directory. Contains only `[model.*]` tables owned by this
script. On each write, tables for providers currently (or previously) in models.dev are
stripped and regenerated from `providers.json`, so deleting a provider also removes its
tables. Unrelated content is not expected in this dedicated file.

## CLI

```
python grok-models.py
python grok-models.py --add-provider
python grok-models.py --config
```

### Default run (sync)

1. Load `providers.json`.
2. Fetch `https://models.dev/api.json` once (live).
3. For each **enabled** provider:
   - Look up the provider in the API. If missing, warn and skip.
   - **Reconcile** its `models` map against the live model list:
     - Add models present in the API but not in `providers.json` as `enabled: false`
       (disabled by default).
     - Remove models in `providers.json` that are no longer in the API (stale).
     - Leave models that are `enabled: false` but still in the API exactly as they are
       (never re-enabled by a sync).
   - For each enabled model, build its TOML fields from the API entry.
   - Warn if the provider has no `api` (base URL) in models.dev (SDK-only providers like
     `anthropic`, `google`); the table gets an empty `base_url`.
4. Persist any changes back to `providers.json`.
5. Backup `models.toml` → `models.toml.bak`, then write tables for all enabled
   providers + enabled models.

### `--add-provider`

1. Ask only for the **provider id**, or let the user type `search` to query the API's
   provider list (filter by id/name, pick one). The chosen id is the concrete
   provider-id.
2. Look up that provider in the API and fetch its model list. Error if not found or has
   no models.
3. Append the provider to `providers.json` with every model `enabled: false` by default
   (no other
   prompts at add time).
4. Offer **`Sync now? [Y]`**. Accepting runs the sync and writes `models.toml`. Declining
   leaves `providers.json` updated without touching `models.toml`.

### `--config`

Interactive. Runs inside a **single curses session** (arrow-key TUI) when stdout is a TTY;
falls back to a **numbered, paged menu** when it is not.

1. **Select a provider** — `↑`/`↓` to move, `Enter` to open, `q` cancels.
2. **Provider action menu**:
   - **Configure models** — opens the model list for that provider.
   - **Disable provider** / **Enable provider** — toggles the whole provider (gates all
     its tables).
   - **Delete provider** — confirm first; removes it from `providers.json` and runs the
     sync immediately so its tables are stripped from `models.toml`.
   - **Back** — return to provider list.
3. **Configure models (curses widget)** — type to filter live, `↑`/`↓` to move,
   `←`/`→` to page, `Enter` toggles the selected model's enabled state, `Backspace`
   clears the filter, `q`/`ESC` finishes. Models show `●` (enabled) or `○` (disabled);
   `free` models get a `[free]` tag. The list is sorted **enabled first, then free
   models, then alphabetical**, with separator lines between enabled | free-disabled |
   rest. Toggling writes `providers.json` immediately; the sync runs after editing so
   `models.toml` reflects the changes.

The numbered fallback has the same behavior: substring filter, `p`/`n` paging (15 per
page), separators, and toggle a model by its number.

## TOML field mapping (from models.dev)

For each enabled model, a `[model.<provider-id>-<sanitized-model-id>]` table is written.
The table key sanitizes `.`, `/`, and `:` to `_` (TOML bare keys cannot contain them;
colons in ids like `gemma4:31b` must also be escaped); the `model` field keeps the
original id.

| TOML field | Source in models.dev |
|---|---|
| `model` | model `id` |
| `base_url` | provider `api` |
| `name` | model `name` + ` (` + provider `name` + `)` |
| `env_key` | provider `env[0]` |
| `api_backend` | always `"chat_completions"` |
| `context_window` | model `limit.context` (optional) |
| `supports_reasoning_effort` | model `reasoning` is true (optional) |
| `reasoning_effort` | default `effort` value (optional) |
| `reasoning_efforts` | derived from `reasoning_options` of type `effort` (optional) |

Optional fields are written only when present in the API. Reasoning mapping:
- `reasoning_options` entries with `type: "effort"` and a `values` list become
  `[[model.*.reasoning_efforts]]` rows (`id`/`value` = the value, `label` =
  First-cap `value` + ` Effort`, one row `default = true`).
- `reasoning_effort` (scalar) = the row with `default = true`.
- `reasoning: true` with only `toggle` (no effort values) sets
  `supports_reasoning_effort = true` and emits no effort rows.

Example:

```toml
[model.opencode-go-hy3]
model = "hy3"
base_url = "https://opencode.ai/zen/go/v1"
name = "Hy3 (OpenCode Go)"
env_key = "OPENCODE_API_KEY"
api_backend = "chat_completions"
supports_reasoning_effort = true
reasoning_effort = "low"
context_window = 256000

[[model.opencode-go-hy3.reasoning_efforts]]
id = "low"
value = "low"
label = "Low Effort"
default = true

[[model.opencode-go-hy3.reasoning_efforts]]
id = "high"
value = "high"
label = "High Effort"
default = false
```

## Constraints

- Python 3, stdlib only. **No `tomlkit` or other third-party packages** — TOML is written
  and validated with the standard library (`tomllib` when available; the write still
  succeeds if it is not).
- All work stays in the project directory; no files under `~/.grok`.
- Hit `https://models.dev/api.json` **live** when the script runs (the bundled
  `models.dev.json` is only a reference snapshot).
- Print a summary: providers synced, models added, models removed, models/providers
  missing (skipped), tables written.
- Exit non-zero on HTTP failure or invalid output.
- After writing, tell the user to **quit and relaunch** Grok Build; a new session in the
  same process will not reload `models.toml`.
