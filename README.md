<div align="center">

# Grok Models (`grok-models`)

**Grok Models** is a TUI for configuring Grok Build and Codex agent harnesses to
use custom models from alternate providers and gateways. Originally designed to
automate the configuration for custom models for Grok Build and later adapted to
transform the same data to configure Codex custom models. 

[Installing the released binary](#installing-the-released-binary) ·
[About](#about) ·
[Grok Build config](#grok-build-config) ·
[Codex config](#codex-config) ·
[Syncing model configs](#syncing-model-configs) ·
[Build and install](#build-and-install) ·
[TUI](#tui)
[Commands](#commands)
[Modified configuration files](#modified-configuration-files)
[Agentic conding](#agentic-coding)

</div>

---



## Installing the released binary

Prebuilt binaries are published for macOS, Linux, and WSL:

```sh
curl -fsSL https://github.com/goldengateagent/grok-models/raw/main/install.sh | bash
grok-models
```



## About

`grok-models` enables provider and gateway models to run in Grok Build
and Codex, alongside Grok and GPT. Besides the closed frontier models, there
are many models offered through providers and gateways. Adding open-weights
models in Grok Build and Codex supplements Grok and GPT agentic coding
especially for token usage. Many free models are available by enabling providers
and putting their api key in your env vars. 

`grok-models` enables agentic coding with providers and gateways like OpenCode
(Zen and Go), OpenRouter and Ollama Cloud with their models in Grok Build and
Codex by automating the custom model configurations. Type-filter for Add Model
allows search across 7,483 models and 207 providers and configures enabled
models in Grok Build and Codex. Provider Model Configuration and Search Models
orders enabled models, free models and then disabled models in the list to
easily manage and find models including free models. When a provider is added
with `grok-models` the required env_key for the api key is displayed. Go to the
gateway site like openrouter.ai or opencode.ai, sign in, get a free api key and
export it in your shell. `openrouter/free` slug routes to various free models
while other individual models such as Ox Alpha/GML 5.3 Flash, DeepSeek V4 Flash,
Hy3, MiniMax M3, Nemotron 3 Ultra and Gemma4 have all done well with agentic
coding and often free.

Enabling open-weights models via `grok-models` gives access to these models in
Grok Build using the `/model` command alongside grok models.

## Grok Build config

`grok-models` is a TUI/CLI for configuring custom model configurations for [Grok Build](https://x.ai/cli). 
This allows the use of other providers and models inside Grok Build besides the
Grok model. These model configurations are stored in `~/.grok/config.toml` or
`$GROK_HOME/config.toml` and can be hard to manage manually. Grok Build does not
allow more than one provider config to list models from so each custom model
must be defined in `config.toml` `[model.*]` tables. 

`grok-models` configures these `[model.*]` tables automatically via a TUI that
finds providers and models from [models.dev](https://models.dev) and the providers api to 
manage which providers and models are enabled. While `models.dev` is used for
discovery of providers and models, the provider's api is used for the authority
on which models are currently active. `models.dev` model data is used to
backfill configuration data for a particular model such as context window and
reasoning levels. The provider/model config is stored in `~/.grok/providers.json` 
and when the TUI/CLI runs a sync, the `config.toml` 
`[model.<provider-id>-<model-id>]` tables are updated.

## Codex config

`grok-models` has a feature to enable Codex custom models, syncing the configured 
model data held in `~/.grok/providers.json` to `~/.codex/config.toml` and 
`~/.codex/<provider>-models.json`. Codex only allows one `model_provider` 
configured in its `config.toml` so in the `grok-models` TUI a toggle allows
selection of which provider (and their enabled models) to sync.

## Syncing model configs

Model and provider selections are made in the TUI/CLI, stored in the
`~/.grok/providers.json`, and then synced to Grok Build (and Codex) config
files. A sync to the config files will occur automatically when the TUI exits,
on the CLI with --sync or via the TUI toggle 'Sync Model Config'. Updating
the currently available models from enabled providers can be done with
'Update Model List' on demand. A full sync on exit runs both, update the 
enabled providers' model list (via their base_url) and write the config 
files to disk (Grok Build, and Codex, if enabled), while the TUI allows 
each to be run independently. 

Keep the TUI open, select an enabled provider, choose which models to enabled for
that provider, then select 'Sync Model Config' to flush the config to disk.
Relaunching Grok Build shows the models in `/model`. Relauching Codex Desktop
will show the Codex enabled model_provider models in the model selector. 

## Build and install



##### Extract from distribution

Download the `.tar.gz` for your platform from the releases page, then:

```
% tar xvf grok-models-1.0.0-<platform>.tar.gz
% cd grok-models-1.0.0-<platform>
% ./install.sh
```

Restart your shell (or run `export PATH="$HOME/.grok-models/bin:$PATH"`), then:

```
% grok-models
```



##### Build from source

```
# macOS
% brew install rust

# Linux / WSL
% sudo apt install rustc cargo

% cd grok-models/rust
% ./build-grok-models.sh
% mkdir -p ~/.grok-models/bin
% cp ./grok-models ~/.grok-models/bin
% export PATH="$HOME/.grok-models/bin:$PATH"
% grok-models
```



##### Make a distribution

```
# creates tar.gz distribution in grok-models/rust/dist for current platform
% grok-models/rust/make-release.sh
```



##### Python Version

```
% cd grok-models
% python grok-models.py
```



##### env_key required by enabled providers is shown in the grok-models TUI

```
# add to your shell or ~/.zshrc for macOS
export OPENCODE_API_KEY="$(< ~/.opencode-key)"
export OPENROUTER_API_KEY="$(< ~/.openrouter-key)"
export OLLAMA_API_KEY="$(< ~/.ollama-key)"
export GMICLOUD_API_KEY="$(< ~/.gmicloud-key)"
export POOLSIDE_API_KEY="$(< ~/.poolside-key)"
```



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

After you select a provider, select Configure Models, then type to filter that
provider’s models by model name or model id. Enter enables or disables a model.
Enabled models are pinned to the top, then free models, then the remainder
sorted by name. Required env var for the `env_key` and sample export are shown.

`providers.json` is written on each edit. `config.toml` is rewritten when you
quit, if anything changed. Update Model List to update the model list in
`providers.json` on demand; the row shows `last_updated`. Select Sync Model
Config to rewrite `config.toml` on demand; the row shows `last_synced`. Ensure
the env_key is exported in your shell and restart Grok Build. Type `/model` and
the new models will appear to select.

## Commands


| Flag                | What it does                                                                                   |
| ------------------- | ---------------------------------------------------------------------------------------------- |
| *(none)*            | Interactive TUI                                                                                |
| `--providers`       | List configured providers                                                                      |
| `--provider ID`     | List that provider’s models                                                                    |
| `--models`          | List enabled models                                                                            |
| `--add-provider ID` | Add a models.dev provider (all models start disabled)                                          |
| `--search TERM`     | Search models.dev providers and add one                                                        |
| `--enable TARGET`   | Enable a provider or `provider/model` (repeatable)                                             |
| `--disable TARGET`  | Disable a provider or `provider/model` (repeatable)                                            |
| `--disable-all`     | Disable every model                                                                            |
| `--sync`            | Reconcile `providers.json` with models.dev and rewrite owned `[model.*]` tables in config.toml |
| `--import`          | Create providers/models from existing `config.toml` `[model.*]` tables                         |
| `-h`, `--help`      | Help                                                                                           |


`--sync` adds new API models as disabled, drops models that left the API, and
never re-enables something you turned off.

## Modified configuration files


| Path                              | Role                                                                  |
| --------------------------------- | --------------------------------------------------------------------- |
| `~/.grok/providers.json`          | Providers and per-model enabled flags                                 |
| `~/.grok/config.toml`             | Grok Build config; only matching `[model.*]` tables are rewritten     |
| `$GROK_HOME`                      | Override the Grok Build home directory (default `~/.grok`)            |
| `~/.codex/config.toml`            | Codex `model_provider` / `model_catalog_json` for the active provider |
| `~/.codex/<provider>-models.json` | Codex model catalog for the synced provider                           |
| `$CODEX_HOME`                     | Override the Codex home directory (default `~/.codex`)                |


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
Export that variable in your shell. After `config.toml` changes, relaunch Grok
Build so it reloads the file to pick up the model list. 

## Agentic Coding

`grok-models` 14,000 lines of code including tests was created in one week with
1 Billion tokens utilizing agentic coding with Grok Build and Ox Alpha (max)
(now GLM 5.3 Flash) free pre-release burning 600M tokens through OpenRouter and
OpenCode Zen with additional 200M tokens from Grok 4.6 (high) via Supergrok and
200M tokens from Hy3 (free through OpenCode Zen) and MiniMax M3 (free through
GMI Cloud). 