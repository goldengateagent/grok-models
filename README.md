# grok-models

A TUI/CLI for configuring custom model configurations for [Grok Build](https://x.ai/cli). 
This allows the use of other providers and models inside Grok Build besides the
grok model. These model configurations are stored in `~/.grok/config.toml` or
`$GROK_HOME/config.toml` and can be hard to manage manually. Grok Build does not
allow more than one provider config to pull models from so each custom model
must be defined in `config.toml`. `grok-models` configures these `[model.*]`
tables automatically via a TUI that finds providers and models from [models.dev](https://models.dev) 
and the providers api to manage which providers and models are enabled. While
`models.dev` is used for discovery of providers and models, the provider's api
is used for the authority on which models are currently active. `models.dev`
model data is used to backfill configuration data for a particular model such
as context window and reasoning levels. The provider/model config is stored in
`~/.grok/providers.json` and when the TUI/CLI runs a sync, the `config.toml`
`[model.<provider-id>-<model-id>]` tables are updated.

## Modifies `$GROK_HOME/config.toml` custom models (important)

This tool **adds, updates, and deletes** `$GROK_HOME/config.toml`
`[model.<provider-id>-<model-id>]` tables whose names match a provider/model it
manages. A sync or enable/disable that drops a model **removes that table** from
`config.toml`.

Custom models you added by hand are left alone **only if their table name does
not match** the pattern `[model.<provider-id>-<model-id>]`. Give them unique
names.

It does not rewrite other config sections (`[cli]`, `[ui]`, and so on).

Provider and model state lives in `~/.grok/providers.json` and is updated 
on provider model refresh and modifications to provider and model enablement. 

Grok Build reads the API key from the provider’s `env_key` (shown in the TUI).
Export that variable in your shell. After `config.toml` changes, relaunch 
Grok Build so it reloads the file to pick up the model list. 

## Quick start

There are two implementations that should behave the same:

- `grok-models` — Rust native binary. `rust/build-grok-models.sh` builds to
  `rust/grok-models`. Copy to `~/bin`, add to `PATH`. Builds on macos, wsl and
  linux.
- `grok-models.py` — Python 3, stdlib only. Run with `python grok-models.py`.

Example CLI: Add OpenCode Go and enable GLM 5.3:

```
grok-models --add-provider opencode-go
grok-models --enable opencode-go/glm-5.3
```

Python: `python grok-models.py` with the same flags.

## TUI

```
grok-models
```

No arguments opens a full-screen TUI to configure providers and models.

The provider list shows your providers. Select a provider to configure it:
enable or disable models, enable or disable the provider, change its base URL,
or delete it. Select Add Provider or Add Model, then type to search models.dev
by name (including `free`) to find a provider or model to enable. Enabling a
model adds the provider if it is not already.

The Enabled Models list shows all currently enabled models across all providers
with the model name, provider name and the `<provider-id>/<model-id>` which can
be used on the command line to enable/disable as well. Press `S` to sort that
list by model name; press `S` again to restore sort by provider name.

The required env var list shows the `env_key` entries required for the currently
enabled providers, green if the variable is set, red if you still need to export
it. Grok Build uses the `env_key` value for the API key to connect to the
provider.

After you select a provider, select Configure models, then type to filter that
provider’s models by model name or model id. Enter enables or disables a model.
Enabled models are pinned to the top, then free models, then the remainder
sorted by name. Required env var for the `env_key` and sample export are shown.

`providers.json` is written on each edit. `config.toml` is rewritten when you
quit, if anything changed. Ensure the env_key is exported in your shell and
restart Grok Build. type `/model` and the new models will appear select.

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
