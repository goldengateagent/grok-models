# grok-models

For [Grok Build](https://x.ai/cli).

A small CLI/TUI for choosing which [models.dev](https://models.dev) providers and
models [Grok Build](https://x.ai/cli) should expose. Enabled models are written as `[model.*]` tables
in Grok Build’s config (`~/.grok/config.toml`, or `$GROK_HOME/config.toml`).

There are two implementations that should behave the same:

- `grok-models` — Rust native binary. `rust/build-grok-models.sh` builds to 
`rust/grok-models`. Copy to `~/bin`, add to `PATH`. Builds on macos, wsl and linux.
- `grok-models.py` — Python 3, stdlib only. Run with `python grok-models.py`.

## Modifies `$GROK_HOME/config.toml` custom models (important)

This tool **adds, updates, and deletes** `$GROK_HOME/config.toml` `[model.<provider-id>-<model-id>]`
tables whose names match a provider/model it manages. A sync or enable/disable
that drops a model **removes that table** from `config.toml`.

Custom models you added by hand are left alone **only if their table name does
not match** the pattern `[model.<provider-id>-<model-id>]`. Give them unique names.

It does not rewrite other config sections (`[cli]`, `[ui]`, and so on).

State lives in `~/.grok/providers.json` (enabled flags, provider list). Model
metadata (base URL, env key, context, reasoning) is fetched live from
`https://models.dev/api.json` at sync time.

Grok Build reads the API key from the provider’s `env_key` (shown in the TUI).
Export that variable in your shell. After `config.toml` changes, quit and
relaunch Grok Build so it reloads the file.

## Quick start

Add OpenCode Go and enable GLM 5.3:

```
grok-models --add-provider opencode-go
grok-models --enable opencode-go/glm-5.3
```

Python: `python grok-models.py` with the same flags.

## TUI

```
grok-models
```

No arguments opens a full-screen menu.

The provider list shows your providers and your enabled models. Select a provider to
configure it: enable or disable models, enable or disable the provider,
change its base URL, or delete it. Select add provider or add model, then
type to search models.dev by name (including `free`) to find a provider or model to enable. 
Enabling a model adds the provider if it is not already.

The Enabled Models list shows all currently enabled models across all providers
with the model name, provider name and the `<provider-id>/<model-id>` which can be used
on the command line to enable/disable as well. 

The required env var list shows the `env_key` entries required for the currently 
enabled providers, green if the variable is set, red if you still need to export it. 
Grok Build uses the `env_key` value for the API key to connect to the provider.

After you select a provider, select Configure models, then type to filter
that provider’s models. Enter enables or disables a model. The count updates
as the filter changes. Enabled models are pinned to the top, then free models, then the 
remainder sorted by name. Required env var for the `env_key` and sample export are shown. 

`providers.json` is written on each edit. `config.toml` is rewritten when you
quit, if anything changed.

## Commands

| Flag | What it does |
|------|----------------|
| *(none)* | Interactive TUI |
| `--providers` | List configured providers |
| `--provider ID` | List that provider’s models |
| `--models` | List enabled models |
| `--add-provider ID` | Add a models.dev provider (all models start disabled) |
| `--search TERM` | Search models.dev providers and add one |
| `--enable TARGET` | Enable a provider or `provider/model` (repeatable) |
| `--disable TARGET` | Disable a provider or `provider/model` (repeatable) |
| `--disable-all` | Disable every model |
| `--sync` | Reconcile `providers.json` with models.dev and rewrite owned `[model.*]` tables |
| `--import` | Create providers/models from existing `config.toml` `[model.*]` tables |
| `-h`, `--help` | Help |

`--sync` adds new API models as disabled, drops models that left the API, and
never re-enables something you turned off.

## Data files and when they are written

| Path | Role |
|------|------|
| `~/.grok/providers.json` | Providers and per-model enabled flags |
| `~/.grok/config.toml` | Grok Build config; only matching `[model.*]` tables are rewritten |
| `$GROK_HOME` | Override the home directory (default `~/.grok`) |

- **Add provider** (TUI or `--add-provider`): writes `providers.json` at once.
  All models start disabled, so nothing new is emitted into `config.toml`.
- **Enable a model** in the TUI: writes `providers.json` on that toggle.
  `config.toml` is not patched per keystroke; leaving the TUI after a change
  runs a full sync (same as `--sync`) and then writes owned `[model.*]` tables.
- **`--enable` / `--disable` / `--sync`**: update `providers.json` and sync
  `config.toml` in that same command.
