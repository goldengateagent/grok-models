#!/usr/bin/env python3
"""Sync Grok Build [model.*] tables from models.dev.

Providers and their models are tracked in `providers.json`. Model metadata
(base URL, env key, context window, reasoning) is taken live from
https://models.dev/api.json so no separate model cache is needed.
"""

from __future__ import annotations

import argparse
import difflib
import re
import os
import copy
import curses
import json
import shutil
import sys
import unicodedata
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

GROK_HOME = Path(os.environ.get("GROK_HOME", Path.home() / ".grok"))
PROVIDERS_PATH = GROK_HOME / "providers.json"
CONFIG_TOML_PATH = GROK_HOME / "config.toml"


def codex_home() -> Path:
    """`$CODEX_HOME` if set, else `~/.codex`. Read at call time."""
    raw = os.environ.get("CODEX_HOME")
    if raw:
        return Path(raw)
    return Path.home() / ".codex"


def codex_config_toml_path() -> Path:
    return codex_home() / "config.toml"
MODELS_DEV_URL = "https://models.dev/api.json"
# When True, add-provider and sync take model ids from GET {base_url}/models
# (OpenAI list). When False, the models.dev provider `models` object is the list.
USE_PROVIDER_MODELS_ENDPOINT = True

TOML_SCALAR_FIELDS = (
    "model",
    "base_url",
    "name",
    "env_key",
    "api_backend",
    "supports_reasoning_effort",
    "reasoning_effort",
    "context_window",
    "description",
)
OPTIONAL_META_FIELDS = (
    "supports_reasoning_effort",
    "reasoning_effort",
    "context_window",
    "reasoning_efforts",
)

_models_dev_api = None


class SyncError(Exception):
    """Fatal sync error; main() maps this to a non-zero exit."""


def utc_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def last_updated_stamp() -> str:
    """Local `MM-DD-YYYY HH:MM AM/PM` for providers.json last_updated."""
    now = datetime.now()
    hour = now.hour % 12 or 12
    ampm = "AM" if now.hour < 12 else "PM"
    return f"{now.month:02d}-{now.day:02d}-{now.year} {hour:02d}:{now.minute:02d} {ampm}"


def fail(message: str) -> None:
    raise SyncError(message)


def atomic_write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    tmp.replace(path)


def dump_json(path: Path, obj: object) -> None:
    atomic_write(path, json.dumps(obj, indent=2, ensure_ascii=False) + "\n")


# Canonical layout for providers.json. Every read and write goes through
# these shapes, so entries come out identical no matter which code path
# (import, add-provider, sync) produced them: fields in canonical order,
# providers alphabetically by display name, models alphabetically by
# display name.
TOP_LEVEL_KEY_ORDER = (
    "include_descriptions",
    "write_codex_config_toml",
    "codex_model_provider",
    "last_updated",
    "last_synced",
    "providers",
    "removed_providers",
)
PROVIDER_KEY_ORDER = (
    "id",
    "name",
    "env_key",
    "npm",
    "base_url",
    "enabled",
    "auth_models_list",
    "models",
)
MODEL_KEY_ORDER = (
    "enabled",
    "name",
    "description",
    "modalities",
    "npm",
    "api_backend",
    "context_window",
    "supports_reasoning_effort",
    "reasoning_effort",
    "reasoning_efforts",
)

INCLUDE_DESCRIPTIONS_DEFAULT = False
WRITE_CODEX_CONFIG_TOML_DEFAULT = False
CODEX_MODEL_PROVIDER_DEFAULT = ""

CODE_PANEL_PAD_X = 1  # horizontal padding inside black code panels

# Provider ids highlighted in the Add Provider screen's "Suggested" section.
# Anything already configured lands in the "Added" section above it; the rest
# are listed unhighlighted below. Single source of truth for both sections.
SUGGESTED_PROVIDER_IDS = ("opencode", "opencode-go", "openrouter", "ollama-cloud", "gmicloud")


def catalog_description(minfo: object) -> str | None:
    """models.dev `description` for a model, or None when absent/empty."""
    if not isinstance(minfo, dict):
        return None
    desc = minfo.get("description")
    if isinstance(desc, str) and desc:
        return desc
    return None


def catalog_modalities(minfo: object) -> dict | None:
    """models.dev `modalities` object, or None when absent/not a dict."""
    if not isinstance(minfo, dict):
        return None
    mods = minfo.get("modalities")
    if isinstance(mods, dict):
        return mods
    return None


def catalog_npm(v: object) -> str | None:
    """models.dev `npm` package string, or None when absent/empty."""
    if not isinstance(v, dict):
        return None
    npm = v.get("npm")
    if isinstance(npm, str) and npm:
        return npm
    return None


def get_api_backend(
    provider_id: str, provider_npm: str | None, model_npm: str | None
) -> str:
    if provider_id in ("openai", "xai", "meta"):
        return "responses"
    npm = model_npm or provider_npm or "@ai-sdk/openai-compatible"
    if npm == "@ai-sdk/openai":
        return "responses"
    if npm == "@ai-sdk/anthropic":
        return "messages"
    return "chat_completions"


def write_api_backend(
    entry: dict, provider_id: str, provider_npm: str | None
) -> None:
    model_npm = entry.get("npm") if isinstance(entry.get("npm"), str) else None
    entry["api_backend"] = get_api_backend(provider_id, provider_npm, model_npm)


def resolve_model_description(
    stored: str | None,
    catalog_models: dict,
    mid: str,
) -> str | None:
    """Live catalog description wins; otherwise keep the stored one."""
    live = catalog_description(catalog_models.get(mid))
    if live is not None:
        return live
    if isinstance(stored, str) and stored:
        return stored
    return None


def order_keys(data: dict, key_order: tuple[str, ...]) -> dict:
    """Rebuild a dict with known keys first in canonical order; any unknown
    keys are preserved after them in their original order."""
    ordered = {key: data[key] for key in key_order if key in data}
    ordered.update(
        {key: value for key, value in data.items() if key not in key_order}
    )
    return ordered


def _provider_sort_key(provider: dict) -> str:
    """Sort providers alphabetically by display name (id as fallback)."""
    return (
        provider.get("name")
        if isinstance(provider, dict) and provider.get("name")
        else (provider.get("id") if isinstance(provider, dict) else "")
    ).lower()


def _model_name_key(item: tuple[str, object]) -> str:
    """Sort models by display name (falling back to the model id)."""
    mid, minfo = item
    name = (
        minfo.get("name")
        if isinstance(minfo, dict) and minfo.get("name")
        else mid
    )
    return str(name).lower()


def order_provider_entry(provider: dict) -> dict:
    """Canonical form of one provider entry: ordered fields, models sorted
    alphabetically by display name."""
    entry = order_keys(provider, PROVIDER_KEY_ORDER)
    models = entry.get("models")
    if isinstance(models, dict):
        entry["models"] = {
            mid: order_keys(minfo, MODEL_KEY_ORDER)
            for mid, minfo in sorted(models.items(), key=_model_name_key)
        }
    return entry


def dump_providers(path: Path, doc: dict) -> None:
    """Single write path for providers.json: this is the only sort. Providers
    A–Z by display name, models A–Z by display name, field key order. The
    in-memory `doc` is updated to match the file so later reads of `doc` are
    file order. Provider dict identities are kept so TUI `selected` stays live."""
    reset_codex_if_invalid(doc)
    providers = doc.get("providers")
    if not isinstance(providers, list):
        providers = []
        doc["providers"] = providers
    for pr in providers:
        if not isinstance(pr, dict):
            continue
        canonical = order_provider_entry(pr)
        pr.clear()
        pr.update(canonical)
    providers.sort(key=_provider_sort_key)
    ordered = order_keys(doc, TOP_LEVEL_KEY_ORDER)
    dump_json(path, ordered)
    doc.clear()
    doc.update(ordered)


def enabled_provider_ids(doc: dict) -> list[str]:
    """Ids of configured providers with enabled=True, in file order."""
    out: list[str] = []
    for p in doc.get("providers") or []:
        if not isinstance(p, dict):
            continue
        pid = p.get("id")
        if isinstance(pid, str) and pid and bool(p.get("enabled", True)):
            out.append(pid)
    return out


def find_provider(doc: dict, pid: str) -> dict | None:
    for p in doc.get("providers") or []:
        if isinstance(p, dict) and p.get("id") == pid:
            return p
    return None


def first_enabled_model_id(provider: dict) -> str | None:
    models = provider.get("models")
    if not isinstance(models, dict):
        return None
    for mid, m in models.items():
        if bool(m.get("enabled", True)) if isinstance(m, dict) else True:
            return mid
    return None


def codex_model_provider_id(doc: dict) -> str:
    raw = doc.get("codex_model_provider", CODEX_MODEL_PROVIDER_DEFAULT)
    return raw if isinstance(raw, str) else ""


def set_codex_selection(doc: dict, pid: str | None) -> None:
    """Persist the Codex provider pick. None / '' disables writing but
    leaves `codex_model_provider` so the next config write can clear the
    previously emitted Codex block once."""
    if pid:
        doc["write_codex_config_toml"] = True
        doc["codex_model_provider"] = pid
    else:
        doc["write_codex_config_toml"] = False


def reset_codex_if_invalid(doc: dict) -> bool:
    """If write is on but the configured provider is missing or disabled,
    turn write off and keep `codex_model_provider` for a one-shot cleanup.
    Does not invent keys when already unset. Returns True when changed."""
    flag = bool(doc.get("write_codex_config_toml", WRITE_CODEX_CONFIG_TOML_DEFAULT))
    pid = codex_model_provider_id(doc)
    if not flag:
        return False
    if pid and pid in enabled_provider_ids(doc):
        return False
    doc["write_codex_config_toml"] = False
    return True


def codex_status_token(doc: dict) -> str:
    """Main-menu state token: provider id, or 'disabled'."""
    if not bool(doc.get("write_codex_config_toml", WRITE_CODEX_CONFIG_TOML_DEFAULT)):
        return "disabled"
    raw = doc.get("codex_model_provider", CODEX_MODEL_PROVIDER_DEFAULT)
    pid = raw if isinstance(raw, str) else ""
    return pid if pid else "disabled"


def codex_models_json_path(provider_id: str) -> Path:
    """Catalog file next to config.toml: `$CODEX_HOME/<id>-models.json` or `~/.codex/<id>-models.json`."""
    return codex_home() / f"{provider_id}-models.json"


def codex_models_json_toml_value(provider_id: str) -> str:
    if os.environ.get("CODEX_HOME"):
        return f"$CODEX_HOME/{provider_id}-models.json"
    return f"~/.codex/{provider_id}-models.json"


def _code_line_segments(
    ln: str, highlight: tuple[int, int, int] | None = None
) -> list[tuple[str, int]]:
    """Syntax-color one code line for the black code panels, following vim's
    sh scheme: yellow symbols (=, quotes, redirections), white strings and
    plain text, cyan comments, green variable names. `highlight` optionally
    recolors a character span (start, end, pair) — used to flag unset env
    variables."""
    # Pair IDs only — callers apply _cp() once. Wrapping color_pair() here
    # and again in _draw_seg_line produced reverse+underline (grey on white).
    text = P.CODE_TEXT
    symbol = P.CODE_SYMBOL
    string = P.CODE_STRING
    comment = P.CODE_COMMENT
    var = P.CODE_VAR
    stripped = ln.lstrip()
    if stripped.startswith("#"):
        return [(ln, comment)]
    n = len(ln)
    attrs = [text] * n
    # Assignment: leading identifier before '=' colors as a variable.
    name_m = re.match(r"(\S+)(\s*=)", ln)
    if name_m:
        for i in range(len(name_m.group(1))):
            attrs[i] = var
    else:
        # Leading command word (echo, pbpaste, …) renders yellow.
        cmd = re.match(r"\s*(\S+)", ln)
        if cmd:
            for i in range(cmd.start(1), cmd.end(1)):
                attrs[i] = symbol
    in_double = False
    in_single = False
    dq_open = -1
    sq_open = -1
    for i, ch in enumerate(ln):
        if ch == "'" and not in_double:
            attrs[i] = symbol
            if in_single:
                for j in range(sq_open + 1, i):
                    attrs[j] = string
                in_single = False
            else:
                in_single = True
                sq_open = i
        elif ch == '"' and not in_single:
            attrs[i] = symbol
            if in_double:
                for j in range(dq_open + 1, i):
                    attrs[j] = string
                in_double = False
            else:
                in_double = True
                dq_open = i
        elif ch in "=<>|;" and not in_single and not in_double:
            attrs[i] = symbol
    if highlight is None:
        empty_m = re.match(r'(\S+)\s*=\s*""', ln)
        if empty_m:
            highlight = (0, empty_m.end(1), P.CODE_ERROR)
    if in_single:
        for j in range(sq_open + 1, n):
            attrs[j] = string
    elif in_double:
        for j in range(dq_open + 1, n):
            attrs[j] = string
    if highlight:
        hs, he, hcolor = highlight
        for i in range(max(0, hs), min(n, he)):
            if attrs[i] in (var, text, string):
                attrs[i] = hcolor
    runs: list[list] = []
    for i, attr in enumerate(attrs):
        if runs and runs[-1][2] == attr and runs[-1][1] == i:
            runs[-1][1] = i + 1
        else:
            runs.append([i, i + 1, attr])
    return [(ln[s:e], a) for s, e, a in runs]


def load_json(path: Path, default: dict) -> dict:
    if not path.exists():
        dump_json(path, default)
        return copy.deepcopy(default)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        fail(f"invalid JSON in {path}: {exc}")
    if not isinstance(data, dict):
        fail(f"{path} must contain a JSON object")
    return data


def load_providers() -> dict:
    data = load_json(PROVIDERS_PATH, {"providers": []})
    data.setdefault("providers", [])
    if not isinstance(data["providers"], list):
        fail("providers.json: 'providers' must be a list")
    return data


def env_api_key(env_key: str) -> str:
    """Value of `env_key` if that env var is set and non-empty."""
    if not env_key:
        return ""
    return os.environ.get(env_key, "") or ""


HTTP_TIMEOUT_SEC = 15


def http_get_json(url: str, api_key: str | None = None) -> object:
    headers = {"User-Agent": "grok-models.py", "Accept": "application/json"}
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"
    req = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_SEC) as resp:
            raw = resp.read()
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")[:300]
        fail(f"HTTP {exc.code} fetching {url}: {body}")
    except TimeoutError:
        fail(f"HTTP timeout fetching {url}")
    except urllib.error.URLError as exc:
        reason = exc.reason
        if isinstance(reason, TimeoutError) or "timed out" in str(reason).lower():
            fail(f"HTTP timeout fetching {url}")
        fail(f"HTTP failure fetching {url}: {reason}")
    try:
        return json.loads(raw.decode("utf-8"))
    except json.JSONDecodeError as exc:
        fail(f"invalid JSON from {url}: {exc}")


def fetch_models_dev() -> dict:
    global _models_dev_api
    if _models_dev_api is None:
        payload = http_get_json(MODELS_DEV_URL)
        if not isinstance(payload, dict):
            fail(f"expected object from {MODELS_DEV_URL}")
        _models_dev_api = payload
    return _models_dev_api


def provider_models_url(base_url: str) -> str:
    return base_url.rstrip("/") + "/models"


def parse_openai_models_list(payload: object) -> list[tuple[str, str | None]] | None:
    """OpenAI-style `{object, data: [{id, name?}]}`. None if unusable/empty."""
    if not isinstance(payload, dict):
        return None
    data = payload.get("data")
    if not isinstance(data, list) or not data:
        return None
    items: list[tuple[str, str | None]] = []
    for row in data:
        if not isinstance(row, dict):
            continue
        mid = row.get("id")
        if not isinstance(mid, str) or not mid:
            continue
        name = row.get("name")
        if not isinstance(name, str) or not name:
            name = None
        items.append((mid, name))
    return items or None


def live_fetch_error_status(detail: str) -> str:
    """TUI / CLI status when GET {base_url}/models fails."""
    if not detail:
        return "error: fetch live model list failed"
    if detail.startswith("error "):
        return detail
    return f"error {detail}"


def _is_http_auth_error(exc: BaseException) -> bool:
    msg = str(exc)
    return msg.startswith("HTTP 401 ") or msg.startswith("HTTP 403 ")


def provider_auth_models_list(provider: dict | None) -> bool:
    """True when this provider's /models list requires an API key."""
    if not isinstance(provider, dict):
        return False
    return provider.get("auth_models_list") is True


def try_fetch_provider_models(
    base_url: str,
    env_key: str = "",
    provider: dict | None = None,
) -> tuple[list[tuple[str, str | None]] | None, str | None]:
    """GET {base_url}/models. Returns (rows, None) or (None, url) on failure.

    Never prints — callers decide whether to surface the URL in the TUI
    status line or as a one-line CLI message.

    If providers.json has auth_models_list true, send Authorization: Bearer.
    Otherwise fetch unauthenticated. On 401/403 with a usable env_key, set
    auth_models_list true and retry with the key. Success leaves the flag
    unchanged. Some public lists hang if a key is sent.
    """
    if not isinstance(base_url, str) or not base_url:
        return None, None
    url = provider_models_url(base_url)
    use_auth = provider_auth_models_list(provider)
    api_key = env_api_key(env_key) if use_auth else ""
    try:
        payload = http_get_json(url, api_key=api_key or None)
    except SyncError as exc:
        if use_auth or not env_api_key(env_key) or not _is_http_auth_error(exc):
            return None, str(exc)
        if isinstance(provider, dict):
            provider["auth_models_list"] = True
        try:
            payload = http_get_json(url, api_key=env_api_key(env_key))
        except SyncError as retry_exc:
            return None, str(retry_exc)
    items = parse_openai_models_list(payload)
    if items is None:
        return None, f"empty or invalid model list from {url}"
    return items, None


def catalog_models_dict(pinfo: dict) -> dict:
    models = pinfo.get("models")
    return models if isinstance(models, dict) else {}


def items_from_catalog(catalog_models: dict) -> list[tuple[str, str | None]]:
    items: list[tuple[str, str | None]] = []
    for mid, minfo in catalog_models.items():
        name = minfo.get("name") if isinstance(minfo, dict) else None
        if not isinstance(name, str) or not name:
            name = None
        items.append((str(mid), name))
    return items


def resolve_model_name(
    live_name: str | None,
    stored_name: str | None,
    catalog_models: dict,
    mid: str,
) -> str | None:
    if isinstance(live_name, str) and live_name:
        return live_name
    if isinstance(stored_name, str) and stored_name:
        return stored_name
    minfo = catalog_models.get(mid)
    if isinstance(minfo, dict):
        name = minfo.get("name")
        if isinstance(name, str) and name:
            return name
    return None


def seed_models_from_items(
    items: list[tuple[str, str | None]],
    catalog_models: dict,
    provider_id: str,
    provider_npm: str | None,
) -> dict:
    models_map: dict = {}
    for mid, live_name in items:
        entry: dict = {}
        name = resolve_model_name(live_name, None, catalog_models, mid)
        if name:
            entry["name"] = name
        minfo = catalog_models.get(mid)
        desc = catalog_description(minfo)
        if desc is not None:
            entry["description"] = desc
        if isinstance(minfo, dict):
            enrich_model_entry(entry, minfo, provider_id, provider_npm)
        if "api_backend" not in entry:
            write_api_backend(entry, provider_id, provider_npm)
        entry["enabled"] = False
        models_map[mid] = entry
    return models_map


def reconcile_models_map(
    models_map: dict,
    items: list[tuple[str, str | None]],
    catalog_models: dict,
    stats: dict,
    provider_id: str,
    provider_npm: str | None,
) -> None:
    """Add/rename/remove models so keys match `items` (authority order)."""
    authority = {mid for mid, _ in items}
    for mid, live_name in items:
        is_new = mid not in models_map
        m = models_map.setdefault(mid, {})
        if not isinstance(m, dict):
            m = models_map[mid] = {}

        # Name: live /models wins, then the stored value, then the catalog.
        stored = m.get("name") if isinstance(m.get("name"), str) else None
        name = resolve_model_name(live_name, stored, catalog_models, mid)
        if name and m.get("name") != name:
            m["name"] = name
            stats["models_renamed"] = stats.get("models_renamed", 0) + 1

        # Fill missing attributes; refresh the description when the catalog
        # carries a different one. User-set values are never overwritten.
        minfo = catalog_models.get(mid)
        if isinstance(minfo, dict):
            enrich_model_entry(m, minfo, provider_id, provider_npm)
            desc = catalog_description(minfo)
            if desc is not None and m.get("description") != desc:
                m["description"] = desc
                stats["descriptions_updated"] = (
                    stats.get("descriptions_updated", 0) + 1
                )
        if "api_backend" not in m:
            write_api_backend(m, provider_id, provider_npm)

        # New entries start disabled.
        if is_new:
            m["enabled"] = False
            stats["models_added"] = stats.get("models_added", 0) + 1

    # Remove entries the authority list no longer carries.
    for mid in list(models_map):
        if mid not in authority:
            del models_map[mid]
            stats["models_removed"] = stats.get("models_removed", 0) + 1


def authority_items_for_provider(
    pinfo: dict,
    base_url: str,
    quiet: bool = False,
    env_key: str = "",
    provider: dict | None = None,
) -> tuple[list[tuple[str, str | None]], str | None]:
    catalog = catalog_models_dict(pinfo)
    if USE_PROVIDER_MODELS_ENDPOINT and base_url:
        live, err = try_fetch_provider_models(
            base_url, env_key=env_key, provider=provider
        )
        if live is not None:
            return live, None
        if err:
            msg = live_fetch_error_status(err)
            if not quiet:
                print(msg)
            return items_from_catalog(catalog), msg
        return items_from_catalog(catalog), err
    return items_from_catalog(catalog), None


def first_letter_cap(text: str) -> str:
    if not text:
        return text
    return text[0].upper() + text[1:]


def api_env_key(pinfo: dict) -> str:
    """First env var name for this provider, from a raw models.dev entry."""
    env = pinfo.get("env")
    if isinstance(env, list) and env and isinstance(env[0], str):
        return env[0]
    return ""


def first_env_key(provider: dict) -> str:
    """Stored env var name from a providers.json provider entry."""
    env_key = provider.get("env_key")
    return env_key if isinstance(env_key, str) else ""


def table_model_id(provider_id: str, live_id: str) -> str:
    # Dots, slashes, and colons nest/break TOML bare keys; Grok table keys use '_'.
    safe = live_id.replace(".", "_").replace("/", "_").replace(":", "_")
    return f"{provider_id}-{safe}"


def parse_bool(raw: str) -> bool | None:
    s = raw.strip().lower()
    if s in ("y", "yes", "true", "1", "on", "enable", "enabled"):
        return True
    if s in ("n", "no", "false", "0", "off", "disable", "disabled"):
        return False
    return None


def prompt_line(label: str, default: str | None = None) -> str:
    if default is None:
        shown = f"{label}: "
    else:
        shown = f"{label} [{default}]: "
    try:
        raw = input(shown)
    except EOFError:
        fail("unexpected end of input")
    if raw.strip() == "" and default is not None:
        return default
    return raw.strip()


def prompt_required(label: str) -> str:
    while True:
        value = prompt_line(label)
        if value:
            return value
        print("Value required.")


def _provider_matches(pid: str, name: str, term_l: str) -> bool:
    """Case-insensitive substring match against provider id or display name."""
    return term_l in pid.lower() or term_l in name.lower()


def search_providers(models_dev: dict, term: str) -> str | None:
    """Search the models.dev provider list with term; return a chosen id."""
    term_l = term.lower()
    matches: list[tuple[str, str]] = []
    for pid, pinfo in models_dev.items():
        if not isinstance(pinfo, dict):
            continue
        name = pinfo.get("name") or ""
        if _provider_matches(pid, name, term_l):
            matches.append((pid, name))
    if not matches:
        print("No providers matched that term.")
        return None
    matches.sort(key=lambda x: x[0])
    shown = matches[:50]
    for i, (pid, name) in enumerate(shown, 1):
        print(f"  {i}. {pid} ({name})")
    if len(matches) > len(shown):
        print(f"  ... and {len(matches) - len(shown)} more")
    while True:
        choice = prompt_line("Select provider (number or id, 'cancel')").strip()
        if choice.lower() == "cancel":
            return None
        if choice.isdigit():
            idx = int(choice)
            if 1 <= idx <= len(shown):
                return shown[idx - 1][0]
        for pid, _ in matches:
            if pid == choice:
                return pid
        print("Pick a listed number or provider id, or 'cancel'.")


_CURSES_FAILED = object()  # sentinel: curses couldn't run (not a tty, etc.)
_SORT_TOGGLED = object()  # sentinel: main-menu S toggled Enabled Models sort
_CURSES_IGNORE = object()  # leaked CSI/mouse after ESC; keep the current screen
# Extra keys decoded from a wheel burst; _curses_getch pops these first.
_curses_key_q: list = []
# DEC private modes matching rust enable_mouse / disable_mouse (X10 + SGR).
_MOUSE_ENABLE = b"\x1b[?1000h\x1b[?1006h"
_MOUSE_DISABLE = b"\x1b[?1000l\x1b[?1006l"
# ncurses NCURSES_MOUSE_MASK(5, PRESSED) = 2 << 24. Homebrew Python's curses
# often omits BUTTON5_PRESSED, which made wheel-down a no-op on macOS.
_BUTTON4_PRESSED = getattr(curses, "BUTTON4_PRESSED", 0x80000)
_BUTTON5_PRESSED = getattr(curses, "BUTTON5_PRESSED", 0x2000000)


# Tokyo Night (Night/Storm) palette — values mirror grok-build's tokyonight.rs.
_TN = {
    "bg": (36, 40, 59),            # #24283b  bg_base (Storm)
    "bg_dark": (31, 35, 53),       # #1f2335
    "bg_highlight": (41, 46, 66),  # #292e42
    "bg_visual": (40, 52, 87),     # #283457  selection
    "fg": (192, 202, 245),         # #c0caf5  text primary
    "fg_dark": (169, 177, 214),    # #a9b1d6  text secondary
    "comment": (86, 95, 137),      # #565f89  muted/gray
    "dark5": (115, 122, 162),      # #737aa2  gray bright
    "blue": (122, 162, 247),       # #7aa2f7  values / accent
    "cyan": (125, 207, 255),       # #7dcfff  free tags / running
    "green": (158, 206, 106),      # #9ece6a  success / on
    "red": (247, 118, 142),        # #f7768e  error / missing key
}

# Color pair ids used by the TUI. Backgrounds are pinned to the theme bg so
# nothing ever renders on the user's terminal default.
class P:
    TEXT = 1        # normal text on theme bg
    MUTED = 2       # gray meta text on theme bg
    VALUE = 3       # blue value text on theme bg
    FREE = 4        # cyan free tag on theme bg
    ENABLED = 5     # green enabled model on theme bg
    DISABLED = 6    # muted disabled model on theme bg
    SELECTED = 7    # bold primary on selection bg
    CHEVRON = 8     # gray chevron on theme bg
    LEGEND_KEY = 9  # bold secondary key in legend
    LEGEND_DESC = 10  # gray description in legend
    ERROR = 11       # red missing/error text on theme bg
    ENABLED_SEL = 12  # green enabled text on selection bg
    ERROR_SEL = 13    # red text on selection bg
    VALUE_SEL = 20    # blue value text on selection bg
    CODE_TEXT = 14    # green on black — code-block body (Homebrew-style)
    CODE_COMMENT = 15  # gray on black — code-block comments
    CODE_ERROR = 16   # red on black — unset env var in code block
    CODE_STRING = 17  # white on black — string constants in code block
    CODE_SYMBOL = 18  # blue on black — symbols (=, quotes) in code block
    CODE_VAR = 19     # green on black — variable names in code block


def _char_cols(ch: str) -> int:
    """Terminal columns for one Unicode scalar (emoji/CJK = 2)."""
    if ch == "➕":
        return 2
    o = ord(ch)
    if 0x1F300 <= o <= 0x1FAFF:
        return 2
    ea = unicodedata.east_asian_width(ch)
    if ea in ("F", "W"):
        return 2
    return 1


def _str_cols(s: str) -> int:
    return sum(_char_cols(c) for c in s)


def _clip_cols(s: str, max_cols: int) -> str:
    out: list[str] = []
    n = 0
    for c in s:
        w = _char_cols(c)
        if n + w > max_cols:
            break
        out.append(c)
        n += w
    return "".join(out)


def _pad_cols(s: str, width: int, fill: str = "\u00a0") -> str:
    s = _clip_cols(s, width)
    return s + fill * max(0, width - _str_cols(s))


def _addstr_cols(stdscr, y: int, x: int, s: str, attr) -> int:
    """addstr advancing x by display width so 2-col glyphs do not skew later cells."""
    cx = x
    for ch in s:
        w = _char_cols(ch)
        try:
            stdscr.addstr(y, cx, ch, attr)
        except curses.error:
            break
        cx += w
    return cx


def _curses_init_colors() -> None:
    """Initialize the Tokyo Night theme.

    Strategy: Apple Terminal (and some others) do not allow palette
    redefinition (can_change_color() is False). On those terminals, named
    colors are THEME-RELATIVE — COLOR_BLACK renders as the theme's own
    near-black, which on a light theme is a soft gray, producing patchwork.
    To guarantee identical rendering everywhere we bypass the curses palette
    entirely and emit 24-bit truecolor escapes for the background, falling
    back to named colors only when truecolor is unavailable.
    """
    global _CURSES_COLORS_READY
    if _CURSES_COLORS_READY:
        return  # re-running init_pair every frame causes visible flicker
    _CURSES_COLORS_READY = True
    curses.start_color()
    try:
        curses.use_default_colors()
    except curses.error:
        pass

    # Named curses colors are theme-relative, so they cannot guarantee our
    # palette. Use 24-bit truecolor pairs when the terminal supports them
    # (COLORTERM=truecolor/24bit — macOS Terminal, iTerm2, Windows Terminal,
    # GNOME Terminal all do); otherwise fall back to theme-relative named
    # colors and accept approximation.

    def rgb(r: int, g: int, b: int) -> int:
        """Allocate a color id rendering exact RGB when the terminal allows it."""
        key = (r, g, b)
        if key in _TRUECOLOR_MAP:
            return _TRUECOLOR_MAP[key]
        slot = -1
        if os.environ.get("COLORTERM", "") in ("truecolor", "24bit"):
            slot = _next_truecolor_slot()
            try:
                curses.init_color(slot, int(r * 1000 / 255), int(g * 1000 / 255), int(b * 1000 / 255))
            except (curses.error, ValueError):
                slot = -1
        resolved = slot if slot >= 0 else _nearest_named(r, g, b)
        _TRUECOLOR_MAP[key] = resolved
        return resolved

    fg = rgb(*_TN["fg"])
    fg_dark = rgb(*_TN["fg_dark"])
    comment = rgb(*_TN["comment"])
    blue = rgb(*_TN["blue"])
    cyan = rgb(*_TN["cyan"])
    green = rgb(*_TN["green"])
    red = rgb(*_TN["red"])
    bg = rgb(*_TN["bg"])
    visual = rgb(*_TN["bg_visual"])

    # Code-block palette: same locked RGB as the Rust app (theme.rs
    # CODE_BG / CODE_TEXT / CODE_COMMENT / CODE_STRING / CODE_SYMBOL_GOLD).
    # Named ANSI colors follow the Terminal profile (Clear Dark/Light remap
    # black and green), so these must not use COLOR_BLACK/GREEN/CYAN/WHITE.
    code_bg = rgb(0, 0, 0)
    code_text = rgb(0, 255, 0)
    code_comment = rgb(0, 255, 255)
    code_var = code_text
    _code_symbol = rgb(255, 204, 0)
    _code_error = red  # Tokyo Night red for unset env vars
    code_string = rgb(255, 255, 255)

    pairs = {
        P.TEXT: (fg, bg),
        P.MUTED: (comment, bg),
        P.VALUE: (blue, bg),
        P.FREE: (cyan, bg),
        P.ENABLED: (green, bg),
        P.DISABLED: (fg_dark, bg),
        P.SELECTED: (fg, visual),
        P.CHEVRON: (comment, bg),
        P.LEGEND_KEY: (fg_dark, bg),
        P.LEGEND_DESC: (comment, bg),
        P.ERROR: (red, bg),
        P.ENABLED_SEL: (green, visual),
        P.ERROR_SEL: (red, visual),
        P.VALUE_SEL: (blue, visual),
        P.CODE_TEXT: (code_text, code_bg),
        P.CODE_COMMENT: (code_comment, code_bg),
        P.CODE_ERROR: (_code_error, code_bg),
        P.CODE_STRING: (code_string, code_bg),
        P.CODE_SYMBOL: (_code_symbol, code_bg),
        P.CODE_VAR: (code_var, code_bg),
    }
    ok_ids: list[int] = []
    for pid, (f, b) in pairs.items():
        try:
            curses.init_pair(pid, f, b)
            ok_ids.append(pid)
        except (curses.error, ValueError):
            # Terminal reports fewer pairs than we need: alias this id to the
            # highest pair that worked instead of leaving it uninitialized
            # (uninitialized pairs render as garbage attrs).
            _PAIR_FALLBACK[pid] = ok_ids[-1] if ok_ids else 0


_TRUECOLOR_SLOT = [100]  # start above the 16 standard ANSI slots
_TRUECOLOR_MAP: dict[tuple, int] = {}
_CURSES_COLORS_READY = False  # palette/pairs initialized once per session
_PAIR_FALLBACK: dict[int, int] = {}  # pair id -> working pair id


def _cp(pid: int):
    """curses color-pair attribute with graceful fallback: terminals that
    ran out of pairs reuse the last pair that initialized successfully."""
    return curses.color_pair(_PAIR_FALLBACK.get(pid, pid))


def _next_truecolor_slot() -> int:
    """Each distinct RGB gets its own stable slot (reused when repeated)."""
    slot = _TRUECOLOR_SLOT[0]
    _TRUECOLOR_SLOT[0] += 1
    return slot


def _nearest_named(r: int, g: int, b: int) -> int:
    """Nearest of the 8 basic ANSI colors to the requested RGB."""
    candidates = {
        curses.COLOR_BLACK: (0, 0, 0),
        curses.COLOR_RED: (205, 0, 0),
        curses.COLOR_GREEN: (0, 205, 0),
        curses.COLOR_YELLOW: (205, 205, 0),
        curses.COLOR_BLUE: (0, 0, 238),
        curses.COLOR_MAGENTA: (205, 0, 205),
        curses.COLOR_CYAN: (0, 229, 238),
        curses.COLOR_WHITE: (229, 229, 229),
    }

    def dist(c):
        cr, cg, cb = candidates[c]
        return (r - cr) ** 2 + (g - cg) ** 2 + (b - cb) ** 2

    return min(candidates, key=dist)


def _query_terminal_bg() -> tuple | None:
    """Query the terminal's current background color via OSC 11.

    Returns (r, g, b) 0-255, or None if unsupported/unreadable.
    Apple Terminal, iTerm2, Windows Terminal, GNOME Terminal respond.
    """
    if not sys.stdout.isatty():
        return None
    import termios, select as sel
    fd = 1
    try:
        old = termios.tcgetattr(fd)
        os.write(fd, b"\033]11;?\007")
        result = b""
        deadline = os.fstat(fd).st_mtime  # placeholder
        # read response with short timeout
        import time
        end = time.time() + 0.25
        while time.time() < end:
            r, _, _ = sel.select([fd], [], [], 0.05)
            if r:
                result += os.read(fd, 256)
                if b"\a" in result or b"\033\\" in result:
                    break
        termios.tcsetattr(fd, termios.TCSADRAIN, old)
        # parse: ]11;rgb:RRRR/GGGG/BBBB
        import re
        m = re.search(rb"\]11;rgb:([0-9a-f]+)/([0-9a-f]+)/([0-9a-f]+)", result)
        if m:
            return tuple(int(m.group(i)[:2], 16) for i in (1, 2, 3))
    except Exception:
        pass
    return None


# Cache: (profile_bg_rgb, transparency 0.0-1.0) -> compensated RGB
_COMPENSATED: dict[tuple, tuple] = {}


def _compensated_bg() -> tuple:
    """Return the SGR bg color that renders as Tokyo Night navy on the
    user's (possibly translucent) terminal profile.

    macOS composites: rendered = alpha*bg_color + (1-alpha)*desktop.
    We can't control the desktop, but we CAN read the profile's own
    background color and transparency via AppleScript. Solving for the
    color that makes the blend equal Tokyo Night navy:
        C = (T - (1-alpha)*B) / alpha
    Only applied on macOS with a Terminal.app profile; other platforms
    return the plain theme bg.
    """
    r, g, b = _TN["bg"]
    key = (r, g, b)
    if key in _COMPENSATED:
        return _COMPENSATED[key]
    result = (r, g, b)
    if sys.platform == "darwin" and shutil.which("osascript"):
        try:
            import subprocess
            script = (
                'tell application "Terminal" to get {background color, '
                'transparency} of settings set "'
            )
            # get the active profile name first
            out = subprocess.run(
                ["osascript", "-e",
                 'tell application "Terminal" to get name of current settings of front window'],
                capture_output=True, text=True, timeout=2,
            )
            profile = out.stdout.strip()
            if profile:
                out = subprocess.run(
                    ["osascript", "-e",
                     f'tell application "Terminal" to get background color of settings set "{profile}"'],
                    capture_output=True, text=True, timeout=2,
                )
                parts = [int(x) // 257 for x in out.stdout.strip().split(", ")]
                prof_bg = tuple(parts)
                out = subprocess.run(
                    ["osascript", "-e",
                     f'tell application "Terminal" to get transparency of settings set "{profile}"'],
                    capture_output=True, text=True, timeout=2,
                )
                alpha = 1.0 - float(out.stdout.strip())  # transparency -> opacity
                if 0 < alpha < 1:
                    # compensate per channel
                    def comp(t, b):
                        c = (t - (1 - alpha) * b) / alpha
                        return max(0, min(255, round(c)))
                    result = (
                        comp(r, prof_bg[0]),
                        comp(g, prof_bg[1]),
                        comp(b, prof_bg[2]),
                    )
        except Exception:
            pass
    _COMPENSATED[key] = result
    return result


_SGR_BG_EMITTED = [False]


def _emit_sgr_bg() -> None:
    """Emit the 24-bit SGR background escape (compensated for translucent
    profiles). Called after every refresh because ncurses may emit \033[m
    resets that clear the terminal's active background mid-frame."""
    r, g, b = _compensated_bg()
    if os.environ.get("COLORTERM", "") in ("truecolor", "24bit"):
        if sys.stdout.isatty():
            os.write(1, f"\033[48;2;{r};{g};{b}m".encode())
            _SGR_BG_EMITTED[0] = True


def _curses_theme_bkgd(stdscr) -> None:
    """Fill the whole window with the theme background, every cell.

    Two layers are needed because neither alone covers all terminals:
    - bkgd() marks every cell (including erase-cleared ones) with the theme
      pair — this is the ncurses-native fill.
    - An explicit addstr sweep paints each row cell-by-cell. On translucent
      macOS profiles (Red Sands, Silver Aerogel, Clear Light) cells only
      touched by erase()/bkgd() can render blended with the desktop behind
      the window; cells carrying an explicit character render opaque. The
      sweep gives blanks a real painted character so they composite the same
      as text lines.
    Re-run after every resize and at the top of each draw loop.
    """
    height, width = stdscr.getmaxyx()
    # NBSP (not plain space): ncurses trims trailing spaces that match the
    # window background and emits an EOL-clear instead -- which resets to the
    # terminal profile's own (translucent) background. A non-blank glyph
    # forces explicit colored output for every column.
    fill_ch = "\u00a0"
    fill = curses.color_pair(P.TEXT)
    for y in range(height):
        try:
            stdscr.addstr(y, 0, fill_ch * width, fill)
        except curses.error:
            pass
    stdscr.bkgd(" ", fill)
    _emit_sgr_bg()


def _curses_draw_header(stdscr, text: str) -> None:
    """Draw the full-width title row on the theme background."""
    height, width = stdscr.getmaxyx()
    try:
        stdscr.addstr(0, 0, "\u00a0" * (width - 1), curses.color_pair(P.SELECTED))
        _addstr_cols(
            stdscr, 0, 2, _clip_cols(text, max(0, width - 4)),
            curses.color_pair(P.SELECTED) | curses.A_BOLD,
        )
    except curses.error:
        pass


def _curses_draw_legend(
    stdscr,
    entries: list[tuple[str, str]],
) -> None:
    """Draw the bottom legend: bold keys (faded '/' separators), gray
    descriptions, │ separators between items.

    entries is a list of (key, description) pairs, e.g. [("←/→", "nav")].
    The full row is painted with the theme background first so the line is
    themed edge to edge, not just where text sits. It is drawn one line up
    from the bottom so the bottom line of the screen stays a blank line of
    padding beneath the menu.
    """
    height, width = stdscr.getmaxyx()
    legend_y = height - 2
    stdscr.addstr(legend_y, 0, " " * (width - 1), curses.color_pair(P.TEXT))
    x = 2
    try:
        for i, (key, desc) in enumerate(entries):
            if i > 0:
                sep = "  │  "
                stdscr.addstr(legend_y, x, sep, curses.color_pair(P.MUTED))
                x += len(sep)
            run = f"{key} {desc}"
            if x + _str_cols(run) > width - 1:
                break
            # Draw the key bold, but render '/' separators faded (muted
            # gray) so they recede against the bold key text (e.g. ↑/↓, Enter/→).
            for ch_k in key:
                if ch_k == "/":
                    attr = curses.color_pair(P.MUTED)
                else:
                    attr = curses.color_pair(P.LEGEND_KEY) | curses.A_BOLD
                stdscr.addstr(legend_y, x, ch_k, attr)
                x += 1
            stdscr.addstr(legend_y, x, " ", curses.color_pair(P.LEGEND_DESC))
            x += 1
            _addstr_cols(stdscr, legend_y, x, desc, curses.color_pair(P.LEGEND_DESC))
            x += _str_cols(desc)
    except curses.error:
        pass


def _pair_on_selection_bg(pid: int) -> int:
    """Keep segment fg, swap theme bg for the selection highlight bg."""
    if pid == P.ENABLED:
        return P.ENABLED_SEL
    if pid == P.VALUE:
        return P.VALUE_SEL
    if pid == P.TEXT:
        return P.SELECTED
    if pid == P.ERROR:
        return P.ERROR_SEL
    return pid


def _draw_seg_line(stdscr, y, x, segments, max_w) -> None:
    """Draw a line of (text, color_pair_id) segments, truncating at max_w."""
    cx = x
    try:
        for text, pid in segments:
            if cx >= x + max_w:
                break
            piece = _clip_cols(text, (x + max_w) - cx)
            if not piece:
                continue
            _addstr_cols(stdscr, y, cx, piece, _cp(pid))
            cx += _str_cols(piece)
    except curses.error:
        pass


def _sgr_wheel_key(btn: int, press: bool, y: int):
    """SGR/X10 wheel: bit 6 marks a wheel event, bit 0 is direction (0 up / 1
    down). Releases (`press == False`) are ignored so a fast wheel does not
    double-step. Matches rust `sgr_wheel_key` from the 7f899b3 wheel fix."""
    if not press or (btn & 64) == 0:
        return _CURSES_IGNORE
    if (btn & 1) == 0:
        return ("wheel_up", y)
    return ("wheel_down", y)


def _parse_key_prefix(buf: list[int]):
    """Parse one key from the front of `buf`.

    Returns `(key, bytes_consumed)` or None when the buffer ends on an
    incomplete escape (caller waits). Unknown CSI is `_CURSES_IGNORE`, never
    Esc — leftover mouse CSI must not pop Configure Models. Mirrors rust
    `parse_key_prefix` (commit 7f899b3)."""
    if not buf:
        return None
    if buf[0] != 27:
        return (buf[0], 1)
    if len(buf) == 1:
        return None
    if buf[1] != ord("["):
        return (27, 1)
    if len(buf) == 2:
        return None
    arrows = {
        ord("A"): curses.KEY_UP,
        ord("B"): curses.KEY_DOWN,
        ord("C"): curses.KEY_RIGHT,
        ord("D"): curses.KEY_LEFT,
    }
    if buf[2] in arrows:
        return (arrows[buf[2]], 3)
    if buf[2] in (ord("5"), ord("6")):
        if len(buf) == 3:
            return None
        if buf[3] == ord("~"):
            key = curses.KEY_PPAGE if buf[2] == ord("5") else curses.KEY_NPAGE
            return (key, 4)
    # SGR mouse: ESC [ < btn ; x ; y M/m
    if buf[2] == ord("<"):
        end = None
        for i in range(3, len(buf)):
            if buf[i] in (ord("M"), ord("m")):
                end = i + 1
                break
        if end is None:
            return None
        press = buf[end - 1] == ord("M")
        payload = bytes(buf[3 : end - 1]).decode("ascii", "replace")
        parts = payload.split(";")
        try:
            btn = int(parts[0]) if parts else 0
        except ValueError:
            btn = 0
        try:
            y = int(parts[2]) if len(parts) >= 3 else 1
        except ValueError:
            y = 1
        y = max(0, y - 1)
        return (_sgr_wheel_key(btn, press, y), end)
    # X10 mouse: ESC [ M Cb Cx Cy (button/x/y each + 32)
    if buf[2] == ord("M"):
        if len(buf) < 6:
            return None
        btn = buf[3] - 32
        y = buf[5] - 32 - 1
        return (_sgr_wheel_key(btn, True, y), 6)
    # Unknown CSI: swallow through its final alpha byte. Never Esc.
    for i in range(2, len(buf)):
        if 65 <= buf[i] <= 90 or 97 <= buf[i] <= 122:
            return (_CURSES_IGNORE, i + 1)
    return (_CURSES_IGNORE, len(buf))


def _as_wheel(ch):
    """Normalize a getch result to `('wheel_up'|'wheel_down', y)` or None."""
    if isinstance(ch, tuple) and ch and ch[0] in ("wheel_up", "wheel_down"):
        return ch
    if ch == getattr(curses, "KEY_MOUSE", -1):
        try:
            _id, _mx, my, _z, bstate = curses.getmouse()
        except curses.error:
            return None
        if bstate & _BUTTON5_PRESSED:
            return ("wheel_down", my)
        if bstate & _BUTTON4_PRESSED:
            return ("wheel_up", my)
    return None


def _curses_getch(stdscr):
    """Read a key, distinguishing a real ESC from a leaked CSI/mouse prefix.

    Fast wheel bursts can outrun ncurses and surface as ESC + leftover bytes.
    Treating those as ESC would pop Configure Models back to the provider
    page. A lone ESC (nothing pending after escdelay) is still ESC.

    SGR (`ESC [<64;x;yM`) and X10 wheel sequences are decoded here so
    scrolling works when Python curses lacks BUTTON5_PRESSED and when
    ncurses cannot parse SGR mouse into KEY_MOUSE."""
    if _curses_key_q:
        return _curses_key_q.pop(0)
    ch = stdscr.getch()
    if ch != 27:
        return ch
    stdscr.nodelay(True)
    pending_special = None
    try:
        buf = [27]
        nxt = stdscr.getch()
        if nxt in (-1, curses.ERR):
            return 27
        mouse = getattr(curses, "KEY_MOUSE", -2)
        if nxt == mouse:
            return nxt
        if nxt > 255:
            return nxt
        buf.append(nxt)
        while True:
            n = stdscr.getch()
            if n in (-1, curses.ERR):
                break
            if n > 255:
                pending_special = n
                break
            buf.append(n)
    finally:
        stdscr.nodelay(False)

    keys = []
    i = 0
    while i < len(buf):
        parsed = _parse_key_prefix(buf[i:])
        if parsed is None:
            # Incomplete at end of burst: a lone ESC is Esc; a truncated
            # CSI/mouse prefix is dropped (not Esc).
            if buf[i] == 27 and len(buf) - i == 1:
                keys.append(27)
            break
        key, used = parsed
        keys.append(key)
        i += used
    if pending_special is not None:
        keys.append(pending_special)
    if not keys:
        return _CURSES_IGNORE
    first, rest = keys[0], keys[1:]
    _curses_key_q.extend(rest)
    return first


CODEX_CONFIG_INFO = (
    "$CODEX_HOME/config.toml and $CODEX_HOME/<provider>-models.json are "
    "updated to enable this provider's enabled models. Codex only allows "
    "one configured provider by setting:\n"
    "\n"
    "  model_provider = <provider>\n"
    "  model_catalog_json = <provider>-models.json\n\n"
    "Disabling removes this config from config.toml and deletes its models json file."
)


def _curses_select_win(
    stdscr,
    options: list[str],
    title: str,
    multi: bool = False,
    preselected: list[int] | None = None,
    back_on_left: bool = False,
    footer: str | None = None,
    initial: int = 0,
    key_hint: str | None = None,
    preview: list | None = None,
    status: str | None = None,
    inline_edit: dict | None = None,
    section_sep_before: int | None = None,
    model_initial: tuple[str, str] | None = None,
) -> int | list[int] | None:
    """curses selector drawn into an existing stdscr with color theme.

    inline_edit (optional): {"row": i, "get": fn, "set": fn} turns row i into
    an in-place text field instead of a selectable action: Enter opens an
    editor inside that row's [...] area (other rows stay visible), typing
    appends, Backspace erases, Enter saves via set(value) and updates the
    label, ESC cancels unchanged. Arrow-right on the row just moves on."""
    curses.set_escdelay(25)
    if not options:
        return None
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    stdscr.leaveok(1)
    _curses_init_colors()
    _curses_theme_bkgd(stdscr)
    state = set(preselected or [])
    n = len(options)
    current = max(0, min(initial, n - 1)) if n > 0 else 0
    top = 0
    # Scroll offset into the preview pane (the enabled-models listing under
    # the provider list). The provider rows above never move.
    preview_scroll = 0
    # Cursor into Enabled Models rows (None = still on the option list).
    model_cursor = None
    if model_initial and preview:
        preview_models = [
            (k, ln[1], ln[2])
            for k, ln in enumerate(preview)
            if isinstance(ln, tuple) and ln[0] == "model"
        ]
        for j, (line_idx, pid, mid) in enumerate(preview_models):
            if (pid, mid) == model_initial:
                model_cursor = j
                current = n - 1 if n else 0
                preview_scroll = line_idx
                break
    while True:
        stdscr.erase()
        height, width = stdscr.getmaxyx()
        safe_w = max(1, width - 1)
        _curses_theme_bkgd(stdscr)
        
        # Header bar
        _curses_draw_header(stdscr, f"  {title}")

        # Codex Config page: explanatory note directly under the heading.
        info_lines = []
        if title.strip() == "Codex Config":
            for para in CODEX_CONFIG_INFO.split("\n"):
                if not para:
                    info_lines.append("")
                    continue
                cur = ""
                for w in para.split(" "):
                    cand = (cur + " " + w).strip() if cur else w
                    if _str_cols(cand) <= max(1, width - 4):
                        cur = cand
                    else:
                        if cur:
                            info_lines.append(cur)
                        cur = w
                if cur:
                    info_lines.append(cur)
        info_h = len(info_lines)
        # Row offset for info text: 2, leaving a blank padding row under the
        # header (row 1). This restores the original main-page layout; Codex
        # Config keeps its note at the same offset.
        info_row_base = 2
        for i, line in enumerate(info_lines):
            try:
                stdscr.addstr(info_row_base + i, 2, _clip_cols(line, width - 3), curses.color_pair(P.MUTED))
            except curses.error:
                pass

        # Codex Config gets an extra blank row between the note and the list
        # of items, matching the Rust layout.
        list_top = info_row_base + info_h + (1 if title.strip() == "Codex Config" else 0)
        list_h = max(1, height - list_top - 2)
        if current < top:
            top = current
        elif current >= top + list_h:
            top = current - list_h + 1
        
        # Optional section rule between the provider rows and the trailing
        # block (Model Descriptions / Add Provider / Add Model). It gets its
        # own screen row and pushes the separator/preview down one line —
        # but only while the whole menu plus the rule fits; otherwise it is
        # skipped and the layout stays exactly as without it.
        rule_row = None  # screen row of the rule, if drawn this frame
        if section_sep_before is not None and n + 1 <= list_h:
            trial_sep = 2 + n + 1  # separator after the shift
            if trial_sep + 1 <= height - 5:
                rule_row = list_top + (section_sep_before - top)
                try:
                    stdscr.addstr(rule_row, 0, "─" * (width - 1), curses.color_pair(P.CHEVRON))
                except curses.error:
                    pass

        env_hdr = "# required env_key values"
        max_env_w = len(env_hdr)
        has_env_cell = False
        for opt in options:
            env = _provider_row_env_text(opt)
            if env:
                has_env_cell = True
                max_env_w = max(max_env_w, len(env))

        for row in range(list_h):
            idx = top + row
            if idx >= n:
                break
            y = list_top + row if rule_row is None or idx < section_sep_before else list_top + row + 1
            opt = options[idx]
            if multi:
                mark = "●" if idx in state else "○"
                line = f"  {mark}  {opt}"
            else:
                line = f"  ▸ {opt}"
            # Clip only when drawing. Tokenizing a width-truncated env cell
            # (cut off before '=') would treat the name as a command word and
            # paint it gold like '='.
            vis = _clip_cols(line, max(1, width - 2))

            is_sel = (idx == current) and model_cursor is None
            try:
                # Row background first (theme bg, or selection bg for the cursor),
                # then the label. A chevron sits right-aligned on expandable rows.
                row_bg = curses.color_pair(P.SELECTED if is_sel else P.TEXT)
                stdscr.addstr(y, 0, "\u00a0" * (width - 1), row_bg)
                label_attr = row_bg
                # Colorize a [enabled]/[disabled] token green/red. The token may
                # be followed by a trailing decorative icon, so locate it by
                # search rather than requiring it at the very end of the line.
                token = None
                tcolor = None
                if "[enabled]" in line:
                    token = "[enabled]"
                    tcolor = P.ENABLED_SEL if is_sel else P.ENABLED
                elif "[disabled]" in line:
                    token = "[disabled]"
                    tcolor = P.ERROR_SEL if is_sel else P.ERROR
                elif "[" in line:
                    start = line.index("[")
                    end = line.find("]", start)
                    if end >= 0 and start + 1 < len(line) and line[start + 1].isdigit():
                        token = line[start : end + 1]
                        tcolor = P.ENABLED_SEL if is_sel else P.ENABLED
                if token:
                    pos = line.index(token)
                    head = line[:pos]
                    tail = line[pos + len(token):]
                    tok_attr = curses.color_pair(tcolor)
                    _addstr_cols(stdscr, y, 0, _clip_cols(head, width - 2), label_attr)
                    hx = _str_cols(head)
                    _addstr_cols(
                        stdscr,
                        y,
                        hx,
                        _clip_cols(token, max(0, (width - 2) - hx)),
                        tok_attr,
                    )
                    if tail:
                        tx = hx + _str_cols(token)
                        nspaces = len(tail) - len(tail.lstrip(" "))
                        env = tail[nspaces:]
                        if nspaces:
                            _addstr_cols(
                                stdscr, y, tx,
                                _clip_cols(" " * nspaces, max(0, (width - 2) - tx)),
                                label_attr,
                            )
                            tx += nspaces
                        if env:
                            box_x = max(0, tx - _PROVIDER_ENV_PAD)
                            box_w = max_env_w + 2 * _PROVIDER_ENV_PAD
                            try:
                                stdscr.addstr(
                                    y, box_x,
                                    " " * min(box_w, max(0, width - 1 - box_x)),
                                    _cp(P.CODE_TEXT),
                                )
                            except curses.error:
                                pass
                            for t, a in _code_line_segments(env):
                                if tx >= width - 2:
                                    break
                                run = _clip_cols(t, max(0, (width - 2) - tx))
                                _addstr_cols(stdscr, y, tx, run, _cp(a))
                                tx += _str_cols(run)
                        elif tail:
                            _addstr_cols(
                                stdscr,
                                y,
                                tx,
                                _clip_cols(tail[nspaces:], max(0, (width - 2) - tx)),
                                label_attr,
                            )
                else:
                    _addstr_cols(
                        stdscr, y, 0,
                        _pad_cols(line, width - 1),
                        label_attr,
                    )
                if not multi:
                    chev_x = max(width - 4, _str_cols(vis) + 2)
                    stdscr.addstr(
                        y,
                        chev_x,
                        "›",
                        curses.color_pair(P.CHEVRON),
                    )
            except curses.error:
                pass

        # Env-column header on unused row 1 (main menu provider rows).
        if not back_on_left and not multi and has_env_cell:
            env_x = None
            for opt in options:
                for tok in ("[enabled]", "[disabled]"):
                    if tok in opt:
                        p = opt.index(tok) + len(tok)
                        rest = opt[p:]
                        nsp = len(rest) - len(rest.lstrip(" "))
                        if rest.lstrip(" "):
                            env_x = _str_cols("  ▸ ") + p + nsp
                            break
                if env_x is not None:
                    break
            if env_x is not None:
                try:
                    segs = _code_line_segments(env_hdr)
                    box_x = max(0, env_x - _PROVIDER_ENV_PAD)
                    box_w = max_env_w + 2 * _PROVIDER_ENV_PAD
                    stdscr.addstr(1, box_x, " " * min(box_w, max(0, width - 1 - box_x)), _cp(P.CODE_TEXT))
                    cx = env_x
                    for t, a in segs:
                        stdscr.addstr(1, cx, t, _cp(a))
                        cx += len(t)
                except curses.error:
                    pass

        # Separator line (pushed down one row while the rule is shown).
        # Skipped on the Codex Config page, which has no footer below it.
        sep_y = list_top + min(n, height - 4)
        if rule_row is not None:
            sep_y += 1
        if title.strip() != "Codex Config":
            try:
                stdscr.addstr(sep_y, 0, "─" * (width - 1), curses.color_pair(P.CHEVRON))
            except curses.error:
                pass

        # Models preview: fill the empty space below the list (the TUI
        # main menu) with the enabled-models listing, styled like --models.
        if preview:
            avail_top = sep_y + 1
            # Locked chrome: H-4 blank, H-3 status, H-2 nav, H-1 blank.
            avail_bottom = height - 5
            max_lines = avail_bottom - avail_top + 1
            if max_lines > 0:
                # Scroll window over the preview; provider rows above stay put.
                # Paging is advertised by the legend's "PgUp/PgDn page" entry,
                # so no inline truncation hint is drawn.
                max_top = max(0, len(preview) - max_lines)
                preview_top = min(preview_scroll, max_top)
                draw_lines = preview[preview_top:preview_top + max_lines]
                for i, segs in enumerate(draw_lines):
                    y = avail_top + i
                    if isinstance(segs, tuple) and segs[0] == "heading":
                        # Full-width blue background bar, like the title.
                        try:
                            stdscr.addstr(y, 0, " " * (width - 1), curses.color_pair(P.SELECTED))
                            stdscr.addstr(
                                y, 4, segs[1][:width - 4],
                                curses.color_pair(P.SELECTED) | curses.A_BOLD,
                            )
                        except curses.error:
                            pass
                    else:
                        draw = segs[3] if isinstance(segs, tuple) and segs[0] == "model" else segs
                        is_model_sel = (
                            model_cursor is not None
                            and isinstance(segs, tuple)
                            and segs[0] == "model"
                            and preview
                            and preview_top + i
                            == [
                                k
                                for k, ln in enumerate(preview)
                                if isinstance(ln, tuple) and ln[0] == "model"
                            ][model_cursor]
                        )
                        if is_model_sel:
                            try:
                                stdscr.addstr(
                                    y, 0, "\u00a0" * (width - 1),
                                    curses.color_pair(P.SELECTED),
                                )
                            except curses.error:
                                pass
                            draw = [(t, _pair_on_selection_bg(p)) for t, p in draw]
                        _draw_seg_line(stdscr, y, 2, draw, width - 3)

        # Transient status line (e.g. post-add confirmation), kept a few rows
        # above the legend so long messages never clobber the menu chrome.
        if status:
            try:
                stdscr.addstr(
                    height - 3,
                    2,
                    status[: max(0, width - 4)],
                    curses.color_pair(P.ENABLED),
                )
            except curses.error:
                pass

        # Footer(s): code panels under the separator — borderless black
        # rectangles, one column of horizontal padding, no vertical padding,
        # vim-sh syntax colors via the shared _code_line_segments tokenizer.
        # Panel 1 = key-setup commands; Panel 2 = env status.
        if footer or key_hint:
            text_attr = _cp(P.CODE_TEXT)
            bx = 2
            legend_y = height - 2
            y = sep_y + 1

            def draw_code_panel(row: int, panel_lines: list[str]) -> None:
                """Solid black panel whose lines are colored by the shared
                _code_line_segments tokenizer (same as the main-menu panel)."""
                panel_segs = [_code_line_segments(ln) for ln in panel_lines]
                panel_w = min(
                    max(sum(len(t) for t, _ in segs) for segs in panel_segs) + 2,
                    max(1, width - bx - 2),
                )
                if row + len(panel_lines) > legend_y:
                    return
                try:
                    for ry in range(row, row + len(panel_lines)):
                        stdscr.addstr(ry, bx, " " * panel_w, text_attr)
                    for i, segs in enumerate(panel_segs):
                        ry = row + i
                        cx = bx + 1
                        for t, a in segs:
                            if cx >= bx + panel_w - 1:
                                break
                            run = t[: bx + panel_w - 1 - cx]
                            stdscr.addstr(ry, cx, run, _cp(a))
                            cx += len(run)
                except curses.error:
                    pass

            panel_lines: list[str] = []
            if key_hint:
                panel_lines.extend(key_hint.split("\n"))
            if footer:
                if panel_lines:
                    panel_lines.append("")
                panel_lines.append("# required env_key value")
                panel_lines.extend(footer.split("\n"))
            if panel_lines:
                draw_code_panel(y, panel_lines)

        # Legend bar
        legend = [("↑/↓", "nav")]
        if multi:
            legend.append(("Space", "toggle"))
        if back_on_left:
            legend.append(("Enter/→", "select"))
            legend.append(("←", "back"))
        else:
            # Main menu: page sits left of select when the preview overflows.
            pane_h = (height - 5) - (sep_y + 1) + 1
            if preview and len(preview) > max(0, pane_h):
                legend.append(("PgUp/PgDn", "page"))
            legend.append(("Enter/→", "select"))
            legend.append(("S", "sort"))
            legend.append(("Q", "quit"))
        _curses_draw_legend(stdscr, legend)
        
        stdscr.refresh()
        _emit_sgr_bg()
        ch = _curses_getch(stdscr)
        if ch is _CURSES_IGNORE:
            continue
        if ch == curses.KEY_RESIZE:
            _curses_theme_bkgd(stdscr)
            continue
        preview_models = [
            (k, ln[1], ln[2])
            for k, ln in enumerate(preview or [])
            if isinstance(ln, tuple) and ln[0] == "model"
        ]
        if ch == curses.KEY_UP:
            if model_cursor is not None:
                if model_cursor > 0:
                    model_cursor -= 1
                    if preview_models[model_cursor][0] < preview_scroll:
                        preview_scroll = preview_models[model_cursor][0]
                else:
                    model_cursor = None
            elif current > 0:
                current -= 1
        elif ch == curses.KEY_DOWN:
            if model_cursor is not None:
                if model_cursor + 1 < len(preview_models):
                    model_cursor += 1
                    # keep the picked model in the preview window
            elif current < n - 1:
                current += 1
            elif preview_models:
                model_cursor = 0
                preview_scroll = 0
                # start at first model line; heading stays pinned above via scroll
                first = preview_models[0][0]
                if first > 0:
                    preview_scroll = 0
        elif multi and ch == ord(" "):
            if current in state:
                state.discard(current)
            else:
                state.add(current)
        elif (
            inline_edit is not None
            and not multi
            and current == inline_edit["row"]
            and ch in (curses.KEY_ENTER, 10, 13, curses.KEY_RIGHT)
        ):
            if ch == curses.KEY_RIGHT:
                continue  # arrows navigate; only Enter activates editing
            # In-place edit of the row's [...] area: no screen erase, so the
            # other rows stay visible. The cursor is an inverted space drawn
            # between the buffer and the closing bracket (terminal-independent).
            buf = list(inline_edit["get"]())
            _, edit_w = stdscr.getmaxyx()
            row_y = list_top + (current - top)
            prefix = options[current].split("[", 1)[0]
            row_prefix = "  ▸ "
            _curses_draw_legend(stdscr, [("Enter", "save"), ("ESC", "cancel")])
            while True:
                open_text = f"{row_prefix}{prefix}[{''.join(buf)}"[: max(1, edit_w - 3)]
                try:
                    stdscr.addstr(
                        row_y, 0, " " * (edit_w - 1), curses.color_pair(P.SELECTED)
                    )
                    stdscr.addstr(
                        row_y,
                        0,
                        open_text,
                        curses.color_pair(P.SELECTED) | curses.A_BOLD,
                    )
                    cur_x = min(len(open_text), max(1, edit_w - 3))
                    stdscr.addstr(
                        row_y,
                        cur_x,
                        " ",
                        curses.color_pair(P.SELECTED) | curses.A_REVERSE,
                    )
                    close_x = min(len(open_text) + 1, max(1, edit_w - 2))
                    stdscr.addstr(
                        row_y,
                        close_x,
                        "]",
                        curses.color_pair(P.SELECTED) | curses.A_BOLD,
                    )
                except curses.error:
                    pass
                stdscr.refresh()
                _emit_sgr_bg()
                e_ch = stdscr.getch()
                if e_ch in (curses.KEY_BACKSPACE, 127, 8):
                    if buf:
                        buf.pop()
                elif 32 <= e_ch <= 126:
                    buf.append(chr(e_ch))
                elif e_ch in (curses.KEY_ENTER, 10, 13):
                    value = "".join(buf).strip()
                    inline_edit["set"](value)
                    options[inline_edit["row"]] = f"{prefix}[{value}]"
                    break
                elif e_ch == 27:  # ESC cancels without changes
                    break
                # anything else (RESIZE etc.) just re-renders the row
            continue
        elif ch in (curses.KEY_ENTER, 10, 13, curses.KEY_RIGHT):
            if model_cursor is not None and preview_models:
                _i, pid, mid = preview_models[model_cursor]
                return ("model", pid, mid)
            return sorted(state) if multi else current
        elif (wheel := _as_wheel(ch)) is not None and preview_models:
            wkind, my = wheel
            avail_top = sep_y + 1
            if not (avail_top <= my <= height - 5):
                continue
            if wkind == "wheel_down":
                preview_scroll = min(preview_scroll + 1, max(0, len(preview or []) - 1))
            else:
                preview_scroll = max(preview_scroll - 1, 0)
            # Pin highlight to the first visible model row.
            vis = [
                j
                for j, (idx, _, _) in enumerate(preview_models)
                if idx >= preview_scroll
            ]
            if vis:
                model_cursor = vis[0]
                current = n - 1
        elif back_on_left and ch == curses.KEY_LEFT:
            return None
        elif ch == 27 and back_on_left:
            # ESC goes back in submenus
            return None
        elif ch == ord("q") and not back_on_left:
            # q quits the tool; only bound at the main menu
            return None
        elif ch in (curses.KEY_NPAGE, curses.KEY_PPAGE):
            # Page the preview pane; the provider list stays pinned above.
            avail_top = sep_y + 1
            avail_bottom = height - 5
            max_lines = avail_bottom - avail_top + 1
            if max_lines > 0:
                max_top = max(0, len(preview or []) - max_lines)
                if ch == curses.KEY_NPAGE:
                    preview_scroll = min(preview_scroll + max_lines, max_top)
                else:
                    preview_scroll = max(preview_scroll - max_lines, 0)
            if model_cursor is not None and preview_models:
                vis = [
                    j
                    for j, (idx, _, _) in enumerate(preview_models)
                    if idx >= preview_scroll
                ]
                if vis:
                    model_cursor = vis[0]
        elif ch in (ord("s"), ord("S")) and not back_on_left:
            return (_SORT_TOGGLED, current)


def _numbered_select(
    options: list[str],
    title: str | None = None,
    allow_cancel: bool = True,
    footer: str | None = None,
) -> int | None:
    """Numbered fallback menu. Returns the chosen index, or None to cancel."""
    if not options:
        return None
    if title:
        print(title)
    for i, opt in enumerate(options, 1):
        print(f"  {i}. {opt}")
    if footer:
        print(footer)
    while True:
        prompt = "Select (number, or 'q' to cancel)" if allow_cancel else "Select (number)"
        choice = prompt_line(prompt)
        if allow_cancel and choice.lower() == "q":
            return None
        if choice.isdigit():
            n = int(choice)
            if 1 <= n <= len(options):
                return n - 1
        print("Invalid selection.")


def _sort_model_indices(ids: list[str], models: dict, filter_query: str | None = None):
    """Return model indices sorted: enabled first, then free models, then
    alphabetical by display name (model id as tiebreaker). Optional substring
    filter matches model id or model display name (case-insensitive).
    Returns (filtered_indices, enabled_count, free_disabled_count)."""
    def is_free(mid: str) -> bool:
        return "free" in mid.lower()
    def is_enabled(mid: str) -> bool:
        m = models.get(mid)
        return bool(m.get("enabled", True)) if isinstance(m, dict) else False
    def display_name(mid: str) -> str:
        m = models.get(mid)
        if isinstance(m, dict):
            n = m.get("name")
            if isinstance(n, str) and n:
                return n
        return mid
    def matches(mid: str) -> bool:
        if filter_query is None:
            return True
        q = filter_query.lower()
        return q in mid.lower() or q in display_name(mid).lower()
    def sort_key(idx: int):
        mid = ids[idx]
        return (
            0 if is_enabled(mid) else 1,
            0 if is_free(mid) else 1,
            display_name(mid).lower(),
            mid.lower(),
        )
    base = [i for i in range(len(ids)) if matches(ids[i])]
    base.sort(key=sort_key)
    enabled_count = sum(1 for i in base if is_enabled(ids[i]))
    free_disabled_count = sum(1 for i in base[enabled_count:] if is_free(ids[i]))
    return base, enabled_count, free_disabled_count


def _filter_list_view_rows(filtered, separators):
    """Visual rows for the filter list. Each separator occupies its own row
    immediately before filtered[sep_idx]; model rows are never overdrawn.
    Returns ('sep', pair) or ('item', model_idx)."""
    sep_at = {idx: pair for idx, pair in separators if 0 < idx < len(filtered)}
    view = []
    for i in range(len(filtered)):
        if i in sep_at:
            view.append(("sep", sep_at[i]))
        view.append(("item", i))
    return view


def _curses_filter_list_win(
    entries: list,
    stdscr,
    *,
    title: str,
    legend: list,
    compute_view,
    render,
    on_enter=None,
    bottom_padding: int = 0,
    status_fn=None,
) -> None:
    """Generic type-to-filter list widget drawn into an existing stdscr.
    compute_view(entries, query) -> (ordered_entries, separators); separators
    is [(index_before_which, color_pair)] and each rule occupies its own row.
    Arrow keys move between models and skip separators. render(entry,
    is_selected) -> (text, color_pair) or a list of (text, color_pair)
    segments.
    on_enter(entry) -> bool: True keeps the window open, False closes it.
    ESC or Left-at-top always closes. bottom_padding reserves that many
    blank themed rows above the legend so a fully-scrolled list never
    touches the menu chrome. status_fn optionally supplies a transient
    confirmation line drawn a few rows above the legend."""
    curses.set_escdelay(25)
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    stdscr.leaveok(1)
    _curses_init_colors()
    query = ""
    current = 0
    top = 0
    snap_to_current = False
    # Cache the computed view so arrow-key navigation (which leaves the query
    # untouched) reuses it instead of re-filtering/sorting the whole catalog
    # every keystroke. After a toggle the recompute runs, the toggled item
    # leaves its old `filtered` index, and the next item in its section
    # slides up to occupy that index. `current` already points at the
    # right neighbor — no adjustment is needed.
    _view_q = None
    _view = None
    _view_dirty = True
    while True:
        if query != _view_q or _view_dirty:
            filtered, separators = compute_view(entries, query)
            _view_q = query
            _view = (filtered, separators)
            _view_dirty = False
        else:
            filtered, separators = _view
        if not filtered:
            current = 0
        elif current >= len(filtered):
            current = len(filtered) - 1
        view = _filter_list_view_rows(filtered, separators)
        # Map filtered-index -> visual-row in O(N) once, then look up
        # `current` in O(1). The previous tuple-equality loop scanned the
        # full view on every frame, which added up over a 10k-row catalog.
        _pos_of = {row[1]: vi for vi, row in enumerate(view) if row[0] == "item"}
        cur_vis = _pos_of.get(current, 0)
        stdscr.erase()
        height, width = stdscr.getmaxyx()
        _curses_theme_bkgd(stdscr)

        # Header with filter
        _curses_draw_header(
            stdscr, f"  {title}  ({len(filtered)})  |  Filter: {query}"
        )

        list_top = 2
        # Locked chrome: H-4 blank, H-3 status, H-2 nav, H-1 blank.
        list_h = max(1, height - list_top - 4 - bottom_padding)
        if snap_to_current:
            top = cur_vis
            if top + list_h > len(view):
                top = max(0, len(view) - list_h)
            snap_to_current = False
        elif cur_vis < top:
            top = cur_vis
        elif cur_vis >= top + list_h:
            top = cur_vis - list_h + 1
        if top and top >= len(view):
            top = max(0, len(view) - list_h)

        if not filtered:
            try:
                stdscr.addstr(2, 0, "  (no matches)", curses.color_pair(P.MUTED))
            except curses.error:
                pass

        for row in range(list_h):
            vis_i = top + row
            if vis_i >= len(view):
                break
            y = 2 + row
            kind = view[vis_i]
            if kind[0] == "sep":
                try:
                    stdscr.addstr(y, 0, "─" * (width - 1), curses.color_pair(kind[1]))
                except curses.error:
                    pass
                continue
            idx = kind[1]
            entry = filtered[idx]
            raw = render(entry, idx == current)
            is_sel = idx == current
            if isinstance(raw, list):
                segs = raw
                fill_pair = P.SELECTED if is_sel else P.TEXT
            else:
                line, row_pair = raw
                segs = [(line, row_pair)]
                fill_pair = row_pair
            if is_sel:
                segs = [(t, _pair_on_selection_bg(p)) for t, p in segs]
            try:
                stdscr.addstr(y, 0, "\u00a0" * (width - 1), curses.color_pair(fill_pair))
                _draw_seg_line(stdscr, y, 0, segs, max(1, width - 2))
            except curses.error:
                pass

        # Transient status line (e.g. post-add confirmation), a few rows
        # above the legend so it never clobbers the list or the chrome.
        if status_fn:
            status = status_fn()
            if status:
                try:
                    stdscr.addstr(
                        height - 3,
                        2,
                        status[: max(0, width - 4)],
                        curses.color_pair(P.ENABLED),
                    )
                except curses.error:
                    pass

        _curses_draw_legend(stdscr, legend)

        stdscr.refresh()
        _emit_sgr_bg()
        ch = _curses_getch(stdscr)
        if ch is _CURSES_IGNORE:
            continue
        if ch == curses.KEY_RESIZE:
            _curses_theme_bkgd(stdscr)
            continue
        wheel = _as_wheel(ch)
        if wheel is not None:
            wkind, my = wheel
            if not (2 <= my < 2 + list_h):
                continue
            if wkind == "wheel_down" and top + 1 < len(view):
                top += 1
            elif wkind == "wheel_up" and top > 0:
                top -= 1
            else:
                continue
            for kind in view[top:]:
                if kind[0] == "item":
                    current = kind[1]
                    break
            continue
        if ch == 27:  # ESC -> back
            return
        if ch == curses.KEY_UP and current > 0:
            current -= 1
        elif ch == curses.KEY_DOWN and current < len(filtered) - 1:
            current += 1
        elif ch == curses.KEY_RIGHT:
            # Page down: full next page, first row of that page selected
            if filtered:
                current = min((current // list_h + 1) * list_h, max(0, len(filtered) - 1))
                snap_to_current = True
        elif ch == curses.KEY_LEFT:
            if current == 0:
                # At the very top of the first page: left goes back
                return
            if current < list_h:
                # Already on the first page: left just goes to its top
                current = 0
                snap_to_current = True
            else:
                # Page up: full previous page, first row selected
                current = ((current // list_h) - 1) * list_h
                snap_to_current = True
        elif ch in (curses.KEY_BACKSPACE, 127, 8):
            query = query[:-1]
            current = 0
            top = 0
        elif ch in (curses.KEY_ENTER, 10, 13):
            if filtered and on_enter is not None:
                if not on_enter(filtered[current]):
                    return
                _view_dirty = True
                # After a toggle, move the cursor one row inside the
                # section the toggled item just left: disable from the
                # enabled side moves up (current - 1), enable from the
                # disabled side moves down (current + 1). The chevron
                # separator marks the boundary between the two
                # sections.
                _chev = next((i for i, p in separators if p == P.CHEVRON), None)
                if _chev is not None and current < _chev:
                    if current > 0:
                        current -= 1
                elif current + 1 < len(filtered):
                    current += 1
        elif 32 <= ch <= 126:
            query += chr(ch)
            current = 0
            top = 0


def _curses_model_search_win(
    ids: list[str], models: dict, stdscr, provider_title: str,
    pid: str, pname: str,
) -> bool:
    """Model picker built on _curses_filter_list_win: type to filter model ids
    or names live, arrow to move, Enter toggles the selected model's enabled
    state, q/ESC finishes. Left/Right arrows page.
    Mutates models in place. Returns True if any toggle happened, False otherwise."""
    changed = False

    def compute_view(entries, query):
        indices, enabled_count, free_disabled_count = _sort_model_indices(ids, models, query)
        ordered = [ids[i] for i in indices]
        separators = []
        if 0 < enabled_count < len(ordered):
            separators.append((enabled_count, P.CHEVRON))
        free_sep_idx = enabled_count + free_disabled_count
        if free_disabled_count > 0 and free_sep_idx < len(ordered):
            separators.append((free_sep_idx, P.FREE))
        return ordered, separators

    def render(mid, _is_sel):
        m = models[mid]
        enabled = bool(m.get("enabled", True)) if isinstance(m, dict) else False
        is_free = "free" in mid.lower()
        mark = "●" if enabled else "○"
        mname = mid
        if isinstance(m, dict):
            n = m.get("name")
            if isinstance(n, str) and n:
                mname = n
        rest = f" ({pname}) - {pid}/{mid}"
        name_pair = P.VALUE if enabled else (P.ENABLED if is_free else P.TEXT)
        mark_pair = P.ENABLED if enabled else P.TEXT
        return [
            ("  ", P.TEXT),
            (mark, mark_pair),
            ("  ", P.TEXT),
            (mname, name_pair),
            (rest, P.TEXT),
        ]

    def toggle(mid):
        nonlocal changed
        m = models[mid]
        if not isinstance(m, dict):
            m = models[mid] = {}
        m["enabled"] = not m.get("enabled", True)
        changed = True
        return True  # stay open

    _curses_filter_list_win(
        ids, stdscr,
        title=f"{provider_title} | Configure Model",
        legend=[("↑/↓/←/→", "nav"), ("ESC", "back"), ("Enter", "toggle"), ("type", "filter")],
        compute_view=compute_view,
        render=render,
        on_enter=toggle,
    )
    return changed

def _curses_confirm_win(stdscr, prompt: str) -> bool:
    """Yes/no prompt drawn into an existing stdscr with color."""
    curses.set_escdelay(25)
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    stdscr.leaveok(1)
    _curses_init_colors()
    stdscr.erase()
    _curses_theme_bkgd(stdscr)
    try:
        _curses_draw_header(stdscr, f"  Confirm: {prompt}")
        legend = [("Y", "yes"), ("N", "no"), ("ESC", "cancel")]
        _curses_draw_legend(stdscr, legend)
    except curses.error:
        pass
    stdscr.refresh()
    _emit_sgr_bg()
    while True:
        ch = _curses_getch(stdscr)
        if ch is _CURSES_IGNORE:
            continue
        if ch in (ord("y"), ord("Y")):
            return True
        if ch in (ord("n"), ord("N"), 27):  # ESC cancels
            return False


def _curses_inline_error_win(stdscr, message: str) -> None:
    """Overlay an error message inside the active curses session; any key dismisses."""
    height, width = stdscr.getmaxyx()
    stdscr.erase()
    _curses_theme_bkgd(stdscr)
    try:
        stdscr.addstr(height // 2, 2, message[: max(0, width - 4)], curses.color_pair(P.DISABLED))
        stdscr.addstr(height // 2 + 2, 2, "Press any key to go back", curses.color_pair(P.MUTED))
        _curses_draw_legend(stdscr, [("any key", "back")])
    except curses.error:
        pass
    stdscr.refresh()
    _emit_sgr_bg()
    stdscr.getch()


def _curses_add_provider_win(providers_doc: dict, providers: list, stdscr) -> bool:
    """Modal: type-to-filter the full models.dev catalog and add a provider.

    The catalog is bucketed into three sections like Configure Models:
    Added (already in providers.json, enabled or disabled), Suggested
    (ids in SUGGESTED_PROVIDER_IDS not yet added) and everything else.
    Added rows are inert (Enter is ignored); adding any other provider
    keeps the modal open so several can be added in one visit. The
    modal closes via ESC or Left at the top of the first page.
    Returns True if at least one provider was added. Fetch/add errors
    surface inline so the surrounding TUI session survives."""
    try:
        models_dev = fetch_models_dev()
    except SyncError as exc:
        _curses_inline_error_win(stdscr, f"Fetch failed: {exc}")
        return False

    # Full catalog — already-added providers stay listed so the sections
    # show what is configured; they are just rendered differently.
    catalog = sorted(
        (pid, pinfo.get("name") or "")
        for pid, pinfo in models_dev.items()
        if isinstance(pinfo, dict)
    )
    suggested = set(SUGGESTED_PROVIDER_IDS)

    result = {"added": None}
    status = {"msg": None}

    def added_ids() -> set:
        return {
            p.get("id")
            for p in providers_doc["providers"]
            if isinstance(p, dict)
        }

    def compute_view(entries, query):
        term_l = query.lower()
        matched = [e for e in entries if _provider_matches(e[0], e[1], term_l)]
        current_added = added_ids()
        bucket = []  # 0 = Added, 1 = Suggested, 2 = others
        for entry in matched:
            if entry[0] in current_added:
                bucket.append(0)
            elif entry[0] in suggested:
                bucket.append(1)
            else:
                bucket.append(2)
        ordered = [e for _, e in sorted(zip(bucket, matched), key=lambda p: (p[0], p[1][0]))]
        separators = []
        n_added = bucket.count(0)
        n_sugg = bucket.count(1)
        if 0 < n_added < len(ordered):
            separators.append((n_added, P.ENABLED))
        free_sep_idx = n_added + n_sugg
        if n_sugg > 0 and free_sep_idx < len(ordered):
            separators.append((free_sep_idx, P.FREE))
        return ordered, separators

    _labels_cache: dict[str, str] = {}
    _cache_providers_len = -1

    def padded_labels() -> dict[str, str]:
        nonlocal _labels_cache, _cache_providers_len
        cur_len = len(providers_doc.get("providers", []))
        if cur_len != _cache_providers_len:
            rows = []
            for pid, name in catalog:
                p = next(
                    (
                        pr
                        for pr in providers_doc.get("providers", [])
                        if isinstance(pr, dict) and pr.get("id") == pid
                    ),
                    None,
                )
                if p is not None:
                    pname = p.get("name") or name or pid
                    enabled = bool(p.get("enabled", True))
                else:
                    pname = name or pid
                    enabled = False
                rows.append((pname, pid, enabled))
            _labels_cache = {pid: lab for (_n, pid, _e), lab in zip(rows, _format_provider_id_rows(rows))}
            _cache_providers_len = cur_len
        return _labels_cache

    def render(entry, _is_sel):
        pid, _name = entry
        is_added = pid in added_ids()
        is_sugg = pid in suggested and not is_added
        label = padded_labels().get(pid, pid)
        if label.endswith("[enabled]"):
            token, tok_pair = "[enabled]", P.ENABLED
        elif label.endswith("[disabled]"):
            token, tok_pair = "[disabled]", P.ERROR
        else:
            token, tok_pair = "", P.TEXT
        head = label[: len(label) - len(token)] if token else label
        name_pair = P.ENABLED if is_added else (P.FREE if is_sugg else P.TEXT)
        return [
            ("  ", P.TEXT),
            (head, name_pair),
            (token, tok_pair),
        ]

    def add(entry):
        pid = entry[0]
        if pid in added_ids():
            return True  # already configured here; inert row
        before = len(providers_doc["providers"])
        try:
            fetch_err_url = add_provider_entry(providers_doc, models_dev, pid, quiet=True)
        except SyncError as exc:
            _curses_inline_error_win(stdscr, f"Add failed: {exc}")
            return True  # stay open
        if fetch_err_url:
            status["msg"] = live_fetch_error_status(fetch_err_url)
        if len(providers_doc["providers"]) > before:
            new_entry = next(
                (
                    p
                    for p in providers_doc["providers"]
                    if isinstance(p, dict) and p.get("id") == pid
                ),
                None,
            )
            providers[:] = [
                p
                for p in providers_doc["providers"]
                if isinstance(p, dict) and p.get("id")
            ]
            n_models = len(new_entry.get("models", {})) if isinstance(new_entry, dict) else 0
            result["added"] = (
                f"Added provider '{pid}' with {n_models} models (all disabled)."
            )
            if fetch_err_url:
                status["msg"] = live_fetch_error_status(fetch_err_url)
            else:
                status["msg"] = result["added"]
        return True  # stay open so more providers can be added

    _curses_filter_list_win(
        catalog, stdscr,
        title="Add Provider",
        legend=[("↑/↓/←/→", "nav"), ("ESC", "cancel"), ("Enter", "add"), ("type", "filter")],
        compute_view=compute_view,
        render=render,
        on_enter=add,
        bottom_padding=0,
        status_fn=lambda: status["msg"],
    )
    return result["added"]


def _combo_enabled(providers_doc: dict, pid: str, mid: str) -> bool:
    """True when (pid, mid) is an enabled model in providers.json."""
    for p in providers_doc.get("providers", []):
        if not isinstance(p, dict) or p.get("id") != pid:
            continue
        mm = p.get("models") if isinstance(p.get("models"), dict) else {}
        m = mm.get(mid)
        return bool(m.get("enabled", True)) if isinstance(m, dict) else False
    return False


def _curses_add_model_win(providers_doc: dict, providers: list, stdscr) -> str | None:
    """Modal: type-to-filter every models.dev model across all providers and
    enable the chosen one. Selecting a model of a provider that has not been
    added yet adds that provider first (all its other models disabled), then
    enables just the chosen model. Already-enabled models stay listed at the
    top (same enabled | free | rest sections as Configure Model) and are
    inert. Returns a confirmation status line for the parent menu, or None.
    Fetch/add errors surface inline so the surrounding TUI session survives."""
    try:
        models_dev = fetch_models_dev()
    except SyncError as exc:
        _curses_inline_error_win(stdscr, f"Fetch failed: {exc}")
        return None

    # Flatten the catalog across every provider; already-enabled combos stay
    # listed so the Enabled section can show what is configured.
    catalog = []
    seen = set()
    for pid, pinfo in models_dev.items():
        if not isinstance(pinfo, dict):
            continue
        pname = pinfo.get("name") or pid
        api_models = pinfo.get("models") if isinstance(pinfo.get("models"), dict) else {}
        for mid, minfo in api_models.items():
            mname = minfo.get("name") if isinstance(minfo, dict) else None
            catalog.append((pid, mid, mname or mid, str(pname)))
            seen.add((pid, mid))
    for p in providers_doc.get("providers", []):
        if not isinstance(p, dict) or not p.get("id"):
            continue
        pid = p.get("id")
        pname = p.get("name") or pid
        mm = p.get("models") if isinstance(p.get("models"), dict) else {}
        for mid0, m0 in mm.items():
            if (pid, mid0) in seen:
                continue
            if not isinstance(m0, dict) or not bool(m0.get("enabled", True)):
                continue
            mname = m0.get("name")
            catalog.append(
                (pid, mid0, mname if isinstance(mname, str) and mname else mid0, str(pname))
            )
            seen.add((pid, mid0))

    result = {"status": None}

    # Cache enabled combos for the lifetime of one redraw so render() does
    # O(1) lookups instead of a linear scan through providers.json per row.
    _enabled_cache: set[tuple[str, str]] = set()
    _cache_providers_len = -1

    def _refresh_cache():
        nonlocal _enabled_cache, _cache_providers_len
        cur_len = len(providers_doc.get("providers", []))
        if cur_len != _cache_providers_len:
            _enabled_cache = {
                (p.get("id"), mid)
                for p in providers_doc.get("providers", [])
                if isinstance(p, dict)
                for mid, m in (p.get("models") if isinstance(p.get("models"), dict) else {}).items()
                if isinstance(m, dict) and bool(m.get("enabled", True))
            }
            _cache_providers_len = cur_len

    def is_enabled(entry):
        _refresh_cache()
        return (entry[0], entry[1]) in _enabled_cache

    def is_free(entry):
        return "free" in entry[1].lower()

    def compute_view(entries, query):
        term_l = query.lower()

        def matches(entry):
            return (
                not term_l
                or term_l in entry[2].lower()  # model display name
                or term_l in entry[1].lower()  # model id
            )

        matched = [e for e in entries if matches(e)]
        matched.sort(
            key=lambda e: (
                0 if is_enabled(e) else 1,
                0 if is_free(e) else 1,
                e[2].lower(),
                e[0],
                e[1],
            )
        )
        enabled_count = sum(1 for e in matched if is_enabled(e))
        free_disabled_count = sum(1 for e in matched[enabled_count:] if is_free(e))
        separators = []
        if 0 < enabled_count < len(matched):
            separators.append((enabled_count, P.CHEVRON))
        free_sep_idx = enabled_count + free_disabled_count
        if free_disabled_count > 0 and free_sep_idx < len(matched):
            separators.append((free_sep_idx, P.FREE))
        return matched, separators

    def render(entry, _is_sel):
        pid, mid, mname, pname = entry
        enabled = is_enabled(entry)
        free = is_free(entry)
        mark = "●" if enabled else "○"
        rest = f" ({pname}) - {pid}/{mid}"
        name_pair = P.VALUE if enabled else (P.ENABLED if free else P.TEXT)
        mark_pair = P.ENABLED if enabled else P.TEXT
        return [
            ("  ", P.TEXT),
            (mark, mark_pair),
            ("  ", P.TEXT),
            (mname, name_pair),
            (rest, P.TEXT),
        ]

    def enable(entry):
        pid, mid, mname, pname = entry
        existing = {
            p.get("id") for p in providers_doc["providers"] if isinstance(p, dict)
        }
        provider = next(
            (
                p
                for p in providers_doc["providers"]
                if isinstance(p, dict) and p.get("id") == pid
            ),
            None,
        )
        nonlocal _cache_providers_len
        if is_enabled(entry):
            # Already enabled: disable it. The model stays in the catalog
            # (from models.dev) so it visibly moves into the disabled
            # section, or the free-disabled section if it matches "free".
            if provider is None:
                _curses_inline_error_win(
                    stdscr, f"Disable failed: provider {pid!r} missing"
                )
                return True  # stay open
            models = provider.setdefault("models", {})
            m = models.get(mid)
            if not isinstance(m, dict):
                m = models[mid] = {}
            m["enabled"] = False
            _cache_providers_len = -1
            dump_providers(PROVIDERS_PATH, providers_doc)
            result["status"] = f"Disabled {mname} ({pname}) - {pid}/{mid}."
            return True  # stay open
        # Disabled: enable it (adding the provider first if it isn't in
        # providers.json yet).
        added = False
        fetch_err_url = None
        if pid not in existing:
            before = len(providers_doc["providers"])
            try:
                fetch_err_url = add_provider_entry(providers_doc, models_dev, pid, quiet=True)
            except SyncError as exc:
                _curses_inline_error_win(stdscr, f"Add failed: {exc}")
                return True  # stay open
            if len(providers_doc["providers"]) > before:
                providers[:] = [
                    p
                    for p in providers_doc["providers"]
                    if isinstance(p, dict) and p.get("id")
                ]
                added = True
            provider = next(
                (
                    p
                    for p in providers_doc["providers"]
                    if isinstance(p, dict) and p.get("id") == pid
                ),
                None,
            )
        if provider is None:
            _curses_inline_error_win(stdscr, f"Enable failed: provider {pid!r} missing")
            return True  # stay open
        models = provider.setdefault("models", {})
        m = models.setdefault(mid, {})
        if not isinstance(m, dict):
            m = models[mid] = {}
        m["enabled"] = True
        # Invalidate the per-redraw enabled-cache so the next compute_view
        # sees the freshly-enabled model and sorts it into the Enabled
        # section, not the disabled section.
        _cache_providers_len = -1
        dump_providers(PROVIDERS_PATH, providers_doc)
        prefix = f"Added provider '{pid}'. " if added else ""
        if fetch_err_url:
            result["status"] = live_fetch_error_status(fetch_err_url)
        else:
            result["status"] = f"{prefix}Enabled {mname} ({pname}) - {pid}/{mid}."
        # Stay open so the user can keep toggling; ESC returns to the main
        # menu, which then shows the last action in its status bar.
        return True

    _curses_filter_list_win(
        catalog, stdscr,
        title="Add Model",
        legend=[
            ("↑/↓/←/→", "nav"),
            ("ESC", "cancel"),
            ("Enter", "enable"),
            ("type", "filter"),
        ],
        compute_view=compute_view,
        render=render,
        on_enter=enable,
        bottom_padding=0,
        status_fn=lambda: result["status"],
    )
    return result["status"]


def _curses_set_reasoning(stdscr, providers_doc: dict, pid: str, mid: str) -> str | None:
    """Popup: pick a reasoning_efforts value; persist default + reasoning_effort."""
    provider = next(
        (p for p in providers_doc.get("providers", [])
         if isinstance(p, dict) and p.get("id") == pid),
        None,
    )
    if provider is None:
        return None
    mm = provider.get("models") if isinstance(provider.get("models"), dict) else {}
    m = mm.get(mid)
    if not isinstance(m, dict):
        return None
    efforts = m.get("reasoning_efforts")
    if not isinstance(efforts, list) or not efforts:
        return "No reasoning levels"
    labels = []
    values = []
    for row in efforts:
        if not isinstance(row, dict):
            continue
        val = row.get("value")
        if not isinstance(val, str) or not val:
            continue
        lab = row.get("label") if isinstance(row.get("label"), str) else val
        if row.get("default"):
            labels.append(f"{lab} [default]")
        else:
            labels.append(lab)
        values.append(val)
    if not values:
        return "No reasoning levels"
    mname = m.get("name") or mid
    pick = _curses_select_win(
        stdscr, labels, f"Reasoning: {mname}", back_on_left=True,
    )
    if pick is None or not isinstance(pick, int) or pick < 0 or pick >= len(values):
        return None
    chosen = values[pick]
    m["reasoning_effort"] = chosen
    for row in efforts:
        if isinstance(row, dict):
            row["default"] = row.get("value") == chosen
    dump_providers(PROVIDERS_PATH, providers_doc)
    return f"Reasoning set to {chosen}"


def _curses_config_flow(providers_doc: dict, providers: list) -> bool | object:
    """Run the whole TUI flow inside ONE curses session so there is no
    terminal-mode flash between menus. Returns True if providers.json
    changed, False if not, or _CURSES_FAILED on any curses error."""
    import curses

    def main(stdscr) -> bool:
        try:
            curses.mousemask(curses.ALL_MOUSE_EVENTS)
        except curses.error:
            pass
        # ncurses mousemask typically enables X10 only. SGR (1006) is what
        # modern terminals emit for the wheel; rust enable_mouse sends both.
        try:
            os.write(1, _MOUSE_ENABLE)
        except OSError:
            pass
        try:
            return _curses_config_loop(stdscr, providers_doc, providers)
        finally:
            try:
                os.write(1, _MOUSE_DISABLE)
            except OSError:
                pass
            try:
                curses.mousemask(0)
            except curses.error:
                pass

    def _curses_config_loop(stdscr, providers_doc, providers) -> bool:
        changed = False
        status_msg = None
        sort_by_name = False
        menu_cursor = 0
        model_focus = None
        while True:
            # Order is providers.json (sorted only on dump).
            providers[:] = [
                p
                for p in providers_doc.get("providers", [])
                if isinstance(p, dict) and p.get("id")
            ]
            ordered = providers
            # Trailing block after a section rule: Codex Config, Model
            # Descriptions toggle, Update Model List, Sync Model Config,
            # then the two add actions.
            descriptions_on = bool(
                providers_doc.get("include_descriptions", INCLUDE_DESCRIPTIONS_DEFAULT)
            )
            labels = _provider_menu_labels(ordered)
            token_col = _provider_state_token_col(ordered)
            cstat = codex_status_token(providers_doc)
            labels.append(_pad_state_label(_CODEX_CONFIG_LABEL, f"[{cstat}]", token_col))
            desc = "enabled" if descriptions_on else "disabled"
            labels.append(_pad_state_label(_MODEL_DESC_LABEL, f"[{desc}]", token_col))
            last_updated = providers_doc.get("last_updated")
            if isinstance(last_updated, str) and last_updated:
                labels.append(
                    _pad_state_label(_UPDATE_LIST_LABEL, f"[{last_updated}]", token_col)
                )
            else:
                labels.append(_UPDATE_LIST_LABEL)
            last_synced = providers_doc.get("last_synced")
            if isinstance(last_synced, str) and last_synced:
                labels.append(
                    _pad_state_label(_SYNC_CONFIG_LABEL, f"[{last_synced}]", token_col)
                )
            else:
                labels.append(_SYNC_CONFIG_LABEL)
            labels.append("➕ Add Provider…")
            labels.append("➕ Add Model…")
            pi = _curses_select_win(
                stdscr, labels, "Select Provider (changes sync on exit)",
                status=status_msg,
                preview=_build_config_models_preview(providers_doc, sort_by_name),
                initial=menu_cursor,
                section_sep_before=len(ordered),
                model_initial=model_focus,
            )
            if isinstance(pi, tuple) and pi and pi[0] is _SORT_TOGGLED:
                sort_by_name = not sort_by_name
                menu_cursor = pi[1]
                continue
            if isinstance(pi, tuple) and pi and pi[0] == "model":
                model_focus = (pi[1], pi[2])
                msg = _curses_set_reasoning(stdscr, providers_doc, pi[1], pi[2])
                if msg:
                    status_msg = msg
                    changed = True
                continue
            model_focus = None
            if pi is None:
                return changed
            if pi == len(ordered):
                # Provider rows share the main provider-list layout.
                enabled = [
                    p
                    for p in providers_doc.get("providers", [])
                    if isinstance(p, dict) and p.get("id") and p.get("enabled", True)
                ]
                values = [None] + [p["id"] for p in enabled]
                choices = ["disabled"] + _provider_menu_labels(enabled)
                current = codex_status_token(providers_doc)
                initial = (
                    0
                    if current == "disabled" or current not in values
                    else values.index(current)
                )
                picked = _curses_select_win(
                    stdscr, choices, "Codex Config", initial=initial, back_on_left=True
                )
                if picked is not None:
                    previous = codex_model_provider_id(providers_doc)
                    is_switch = (
                        values[picked] is not None
                        and previous
                        and previous != "disabled"
                        and values[picked] != previous
                    )
                    if is_switch:
                        # Flush the previous pick: turn writing off, sync
                        # (which deletes the previous catalog via the
                        # one-shot cleanup), then turn writing back on for
                        # the new pick and sync again.
                        set_codex_selection(providers_doc, None)
                        dump_providers(PROVIDERS_PATH, providers_doc)
                        update_config_toml(quiet=True)
                        set_codex_selection(providers_doc, values[picked])
                        dump_providers(PROVIDERS_PATH, providers_doc)
                        update_config_toml(quiet=True)
                    else:
                        set_codex_selection(providers_doc, values[picked])
                        dump_providers(PROVIDERS_PATH, providers_doc)
                        update_config_toml(quiet=True)
                    fresh = load_providers()
                    providers_doc.clear()
                    providers_doc.update(fresh)
                    status_msg = f"Codex Config {codex_status_token(providers_doc)}"
                    changed = True
                menu_cursor = pi
                continue
            if pi == len(ordered) + 1:
                new_val = not descriptions_on
                providers_doc["include_descriptions"] = new_val
                dump_providers(PROVIDERS_PATH, providers_doc)
                status_msg = f"Model Descriptions {'enabled' if new_val else 'disabled'}"
                changed = True
                menu_cursor = pi  # stay on the toggle row, like Configure Models
                continue
            if pi == len(ordered) + 2:
                try:
                    stats = update_providers_json(quiet=True)
                    fresh = load_providers()
                    providers_doc.clear()
                    providers_doc.update(fresh)
                    n_sync = stats.get("providers_synced", 0)
                    errs = stats.get("live_fetch_errors") or []
                    if len(errs) == 1:
                        status_msg = errs[0]
                    elif len(errs) > 1:
                        status_msg = f"{errs[0]} (+{len(errs) - 1} more)"
                    else:
                        status_msg = f"Updated model list · {n_sync} providers synced"
                    changed = True
                except SyncError as exc:
                    status_msg = str(exc) if str(exc).startswith("error ") else f"error {exc}: fetch live model list failed"
                menu_cursor = pi
                continue
            if pi == len(ordered) + 3:
                try:
                    update_config_toml(quiet=True)
                    fresh = load_providers()
                    providers_doc.clear()
                    providers_doc.update(fresh)
                    status_msg = "Synced model config"
                except SyncError as exc:
                    status_msg = str(exc) if str(exc).startswith("error ") else f"error {exc}: sync model config failed"
                menu_cursor = pi
                continue
            if pi == len(ordered) + 4:
                added_msg = _curses_add_provider_win(providers_doc, providers, stdscr)
                if added_msg:
                    status_msg = added_msg
                    changed = True
                menu_cursor = pi
                continue
            if pi == len(ordered) + 5:
                enabled_msg = _curses_add_model_win(providers_doc, providers, stdscr)
                if enabled_msg:
                    status_msg = enabled_msg
                    changed = True
                menu_cursor = pi
                continue
            status_msg = None
            selected = ordered[pi]
            menu_cursor = pi
            action_cursor = 0
            while True:
                enabled = bool(selected.get("enabled", True))

                def _bu_get() -> str:
                    v = selected.get("base_url")
                    return v if isinstance(v, str) else ""

                def _bu_set(v: str) -> None:
                    # Empty input removes the override entirely.
                    if v:
                        selected["base_url"] = v
                    else:
                        selected.pop("base_url", None)
                    dump_providers(PROVIDERS_PATH, providers_doc)

                bu_before = _bu_get()
                actions = [
                    "Configure Models",
                    f"Provider [{'enabled' if enabled else 'disabled'}]",
                    f"Base Url [{_bu_get()}]",
                    "Delete Provider",
                    "Back",
                ]
                env_key = first_env_key(selected)
                ai = _curses_select_win(
                    stdscr,
                    actions,
                    f"Provider: {selected.get('name') or selected['id']}",
                    back_on_left=True,
                    initial=action_cursor,
                    inline_edit={
                        "row": 2,
                        "get": _bu_get,
                        "set": _bu_set,
                        "label": lambda v: f"Base Url [{v}]",
                    },
                    footer=(
                        _env_status_line(env_key)
                        if env_key
                        else None
                    ),
                    key_hint=(
                        f"# config {selected.get('id') or selected.get('name')} api keys\n"
                        f"pbpaste > key-file\n"
                        f"echo 'export {env_key}=\"$(cat ~/key-file)\"' >> ~/.zshrc"
                        if env_key
                        else None
                    ),
                )
                # Detect an inline base_url edit before any early exit: the
                # setter already persisted, this only flags the exit sync.
                if _bu_get() != bu_before:
                    changed = True
                if ai is None or actions[ai] == "Back":
                    break
                action_cursor = ai
                if ai == 0:
                    if _curses_model_search_win(
                        list(selected["models"].keys()),
                        selected["models"],
                        stdscr,
                        f"Provider: {selected.get('name') or selected['id']}",
                        selected["id"],
                        selected.get("name") or selected["id"],
                    ):
                        dump_providers(PROVIDERS_PATH, providers_doc)
                        changed = True
                elif ai == 1:
                    selected["enabled"] = not enabled
                    dump_providers(PROVIDERS_PATH, providers_doc)
                    changed = True
                elif ai == 3:
                    if _curses_confirm_win(stdscr, f"Delete Provider {_provider_display(selected)}?"):
                        # Grab the enabled model ids from providers.json
                        # before the entry is removed.
                        enabled = enabled_model_ids(selected)
                        providers_doc["providers"] = [
                            p
                            for p in providers_doc["providers"]
                            if p.get("id") != selected["id"]
                        ]
                        _record_removed_provider(providers_doc, selected["id"], enabled)
                        providers[:] = [
                            p for p in providers if p.get("id") != selected["id"]
                        ]
                        dump_providers(PROVIDERS_PATH, providers_doc)
                        # Flush the deletion into config.toml now so a re-add
                        # of the same provider this session can't collide
                        # with a pending deletion record.
                        update_config_toml(quiet=True)
                        changed = True
                    menu_cursor = 0
                    break
        return changed

    try:
        return curses.wrapper(main)
    except Exception as exc:
        # Surface why the TUI failed instead of silently degrading to the
        # numbered fallback (e.g. terminfo/color issues on some themes).
        import traceback
        traceback.print_exc()
        return _CURSES_FAILED


def _config_models_numbered(ids: list[str], models: dict) -> bool:
    """Numbered fallback for model configuration with substring filtering
    and paging (so long result lists never run off screen). Models sorted:
    enabled first, then free models, then alphabetical. Separator lines
    mark the boundaries: enabled | free-disabled | rest."""
    changed = False
    PAGE = 15
    while True:
        q = prompt_line("Filter substring (empty = all, 'q' done)").strip()
        if q.lower() == "q":
            return changed
        matches, enabled_count, free_disabled_count = _sort_model_indices(ids, models, q)
        if not matches:
            print("No matches.")
            continue
        page = 0
        while True:
            total = len(matches)
            start = page * PAGE
            end = min(total, start + PAGE)
            for n, i in enumerate(matches[start:end], start + 1):
                mid = ids[i]
                m = models[mid]
                enabled = bool(m.get("enabled", True)) if isinstance(m, dict) else False
                # Draw separator before the first disabled on this page
                if n == start + enabled_count and enabled_count < total:
                    print("  " + "─" * 40)
                # Draw separator before the first non-free disabled on this page
                free_sep_idx = enabled_count + free_disabled_count
                if n == start + free_sep_idx and free_disabled_count > 0 and free_sep_idx < total:
                    print("  " + "─" * 40)
                print(f"  {n}. [{'x' if enabled else ' '}] {mid}")
            more = end < total
            nav = []
            if page > 0:
                nav.append("p: prev")
            if more:
                nav.append("n: next")
            nav_hint = f"  ({'  '.join(nav)})" if nav else ""
            raw = prompt_line(
                f"Toggle a number{nav_hint}  (Enter for new filter)"
            ).strip()
            if raw == "":
                break
            if raw.lower() == "n" and more:
                page += 1
                continue
            if raw.lower() == "p" and page > 0:
                page -= 1
                continue
            if raw.isdigit():
                n = int(raw)
                if start + 1 <= n <= end:
                    i = matches[n - 1]
                    m = models[ids[i]]
                    if not isinstance(m, dict):
                        m = models[ids[i]] = {}
                    m["enabled"] = not m.get("enabled", True)
                    changed = True
                    continue
            if raw.lower() == "q":
                return changed
            print("Invalid selection.")


def _provider_label(p: dict) -> str:
    state = "enabled" if p.get("enabled", True) else "disabled"
    return f"({p.get('name') or p['id']}) - {p['id']} [{state}]"


def _provider_display(p: dict) -> str:
    """Main-list identity: `(name) - id`."""
    pid = p.get("id") or ""
    name = p.get("name") or pid
    return f"({name}) - {pid}"


def _format_provider_id_rows(rows: list[tuple[str, str, bool]]) -> list[str]:
    """Padded `(name) - id [enabled/disabled]` rows (no env cell)."""
    names = [f"({name})" for name, _pid, _en in rows]
    name_w = max((len(n) for n in names), default=0)
    id_w = max((len(pid) for _n, pid, _en in rows), default=0)
    token_col = (name_w + 3 + id_w + 1) if rows else 0
    out = []
    for (_name, pid, enabled), nlab in zip(rows, names):
        token = "[enabled]" if enabled else "[disabled]"
        head = f"{nlab.ljust(name_w)} - {pid.ljust(id_w)}"
        out.append(f"{head.ljust(token_col)}{token}")
    return out


# `[disabled]` is the longer state token; pad `[enabled]` to this width so
# the env column starts on one vertical line.
_PROVIDER_TOKEN_W = len("[disabled]")
_PROVIDER_ENV_GAP = 2
_PROVIDER_ENV_PAD = 1
_MODEL_DESC_LABEL = "Model Descriptions"
_CODEX_CONFIG_LABEL = "Codex Config"
_UPDATE_LIST_LABEL = "Update Model List"
_SYNC_CONFIG_LABEL = "Sync Model Config"


def _provider_row_env_text(opt: str) -> str | None:
    """Env-cell text on a main-menu provider row (`ENV = value`), if any."""
    for tok in ("[enabled]", "[disabled]"):
        if tok in opt:
            rest = opt[opt.index(tok) + len(tok) :].lstrip(" ")
            return rest or None
    return None


def _provider_state_token_col(providers: list) -> int:
    """Column where `[enabled]` / `[disabled]` / `[date]` start on the main
    menu. Shared by provider rows and the Model Descriptions / Update Model
    List trailing rows so the tokens form one vertical line."""
    names = [f"({p.get('name') or p['id']})" for p in providers]
    name_w = max((len(n) for n in names), default=0)
    id_w = max((len(p["id"]) for p in providers), default=0)
    provider_col = (name_w + 3 + id_w + 1) if providers else 0
    return max(
        provider_col,
        len(_MODEL_DESC_LABEL) + 1,
        len(_CODEX_CONFIG_LABEL) + 1,
        len(_UPDATE_LIST_LABEL) + 1,
        len(_SYNC_CONFIG_LABEL) + 1,
    )


def _pad_state_label(label: str, token: str, token_col: int) -> str:
    return f"{label.ljust(token_col)}{token}"


def _provider_menu_labels(providers: list) -> list[str]:
    """Padded main-menu provider rows: aligned dashes, aligned state tokens,
    then a gap + env cell."""
    names = [f"({p.get('name') or p['id']})" for p in providers]
    name_w = max((len(n) for n in names), default=0)
    id_w = max((len(p["id"]) for p in providers), default=0)
    token_col = _provider_state_token_col(providers)
    env_w = max(
        (len(first_env_key(p)) for p in providers if first_env_key(p)),
        default=0,
    )
    rows = []
    for p, name in zip(providers, names):
        state = "enabled" if p.get("enabled", True) else "disabled"
        token = f"[{state}]"
        head = f"{name.ljust(name_w)} - {p['id'].ljust(id_w)}"
        left = f"{head.ljust(token_col)}{token.ljust(_PROVIDER_TOKEN_W)}"
        envk = first_env_key(p)
        if envk:
            left = (
                left
                + (" " * _PROVIDER_ENV_GAP)
                + envk.ljust(env_w)
                + " = "
                + _env_value(envk)
            )
        rows.append(left)
    return rows


def _provider_state_line(p: dict) -> str:
    penabled = bool(p.get("enabled", True))
    marker = "●" if penabled else "○"
    return (
        f"{marker} ({p.get('name') or p['id']}) - {p['id']}"
        f"  [{'enabled' if penabled else 'disabled'}]"
    )


def render_list_text(
    providers_doc: dict,
    provider_filter: str | None,
    providers_only: bool = False,
) -> None:
    providers = [
        p for p in providers_doc.get("providers", [])
        if isinstance(p, dict) and p.get("id")
    ]
    if provider_filter is not None and provider_filter not in [
        p["id"] for p in providers
    ]:
        hints = difflib.get_close_matches(provider_filter, [p["id"] for p in providers])
        hint = f" (did you mean: {', '.join(hints)}?)" if hints else ""
        fail(f"unknown provider {provider_filter!r}{hint}")
    print("Configured providers")
    if not providers:
        print("No providers configured yet. Add with --add-provider")
        return

    shown_providers = providers
    if provider_filter is not None:
        by_id = {p["id"]: p for p in providers}
        shown_providers = [by_id[provider_filter]]

    if providers_only and provider_filter is None:
        enabled_providers = 0
        for i, provider in enumerate(shown_providers):
            penabled = bool(provider.get("enabled", True))
            if penabled:
                enabled_providers += 1
            code = "bold" if penabled else None
            print(_provider_state_line(provider))
            env = first_env_key(provider)
            if env:
                print(f"    {_env_status_line(env)}")
        print()
        print(
            f"Summary: {len(shown_providers)} providers · "
            f"{enabled_providers} enabled"
        )
        return

    total_models = 0
    enabled_models = 0
    enabled_providers = 0
    for i, provider in enumerate(shown_providers):
        if i > 0:
            print()
        pid = provider["id"]
        pname = provider.get("name") or pid
        penabled = bool(provider.get("enabled", True))
        if penabled:
            enabled_providers += 1

        models_map = provider.get("models")
        ids = list(models_map.keys()) if isinstance(models_map, dict) else []
        en_count = sum(
            1
            for mid in ids
            if isinstance(models_map.get(mid), dict)
            and models_map[mid].get("enabled", True)
        ) if isinstance(models_map, dict) else 0
        total_models += len(ids)
        enabled_models += en_count

        marker = "●" if penabled else "○"
        line = (
            f"{marker} ({pname}) - {pid}  [{'enabled' if penabled else 'disabled'}]"
            f"  {en_count}/{len(ids)} models"
        )
        if penabled:
            print(line)
        else:
            print(line)

        if not ids:
            print("    (no models)")
            continue
        for mid in ids:
            m = models_map.get(mid) if isinstance(models_map, dict) else None
            menabled = bool(m.get("enabled", True)) if isinstance(m, dict) else False
            free_tag = "  [free]" if "free" in mid.lower() else ""
            mline = f"    {'●' if menabled else '○'} {mid}{free_tag}"
            code = "green" if menabled else "dim"
            print(mline)

    summary = (
        f"Summary: {len(shown_providers)} providers · {enabled_providers} enabled · "
        f"{enabled_models}/{total_models} models enabled"
    )
    print()
    print(summary)


def _env_value(env_var: str) -> str:
    """Current value of an env var: first 10 chars + ellipsis, or empty string."""
    val = os.environ.get(env_var, "")
    return f'"{val[:10]}..."' if val else '""'


def _env_status_line(env_var: str) -> str:
    """Format an env var requirement with its current value status."""
    if not env_var:
        return ""
    val = os.environ.get(env_var, "")
    shown = f'"{val[:10]}..."' if val else '""'
    return f"{env_var} = {shown}"


def enabled_provider_env_vars(providers_doc: dict) -> list[str]:
    """Return the required API-key env vars for all enabled providers."""
    env_vars = []
    for p in providers_doc.get("providers", []):
        if not isinstance(p, dict) or not p.get("id"):
            continue
        if not p.get("enabled", True):
            continue
        env = first_env_key(p)
        if env and env not in env_vars:
            env_vars.append(env)
    return env_vars


def print_env_requirements(providers_doc: dict) -> None:
    """List each enabled provider's required env var and whether it is set."""
    env_vars = enabled_provider_env_vars(providers_doc)
    if not env_vars:
        return
    print()
    print("Required environment variables:")
    for env_var in env_vars:
        print(f"  {_env_status_line(env_var)}")


def render_models_text() -> int:
    providers_doc = load_providers()
    providers = [
        p for p in providers_doc.get("providers", [])
        if isinstance(p, dict) and p.get("id")
    ]

    print("Enabled models")

    total_enabled = 0
    for provider in providers:
        pid = provider["id"]
        penabled = bool(provider.get("enabled", True))
        mm = provider.get("models")
        if not isinstance(mm, dict):
            continue
        pname = provider.get("name") or pid
        for mid, m in mm.items():
            if not isinstance(m, dict) or not m.get("enabled", True):
                continue
            if not penabled:
                continue
            mname = m.get("name") or mid
            print(f"● {mname} ({pname}) - {pid}/{mid}")
            total_enabled += 1

    if not total_enabled:
        print("No enabled models. Enable with --enable or grok-models")
        return 0

    print()
    env_rows = []
    for provider in providers:
        if not bool(provider.get("enabled", True)):
            continue
        env = first_env_key(provider)
        if env:
            pname = provider.get("name") or provider["id"]
            env_rows.append((env, _env_value(env), pname))
    if env_rows:
        maxlen = max(len(e) for e, _, _ in env_rows)
        for env, value, pname in env_rows:
            print(f"● {env:<{maxlen}} = {value}  ({pname})")
    print(f"Summary: {total_enabled} models enabled")
    return 0


def _model_entry(providers: list, pid: str, mid: str) -> dict:
    for p in providers:
        if isinstance(p, dict) and p.get("id") == pid:
            mm = p.get("models") if isinstance(p.get("models"), dict) else {}
            m = mm.get(mid)
            return m if isinstance(m, dict) else {}
    return {}


def _model_reasoning_level(m: dict) -> str:
    efforts = m.get("reasoning_efforts")
    if isinstance(efforts, list):
        for row in efforts:
            if isinstance(row, dict) and row.get("default"):
                v = row.get("value")
                if isinstance(v, str) and v:
                    return v
    v = m.get("reasoning_effort")
    if isinstance(v, str) and v:
        return v
    return "none"


def _build_config_models_preview(
    providers_doc: dict, sort_by_name: bool = False
) -> list:
    """Build the --models-style enabled-models listing as colored segment
    lines, for rendering in the empty space under the TUI main menu.
    Default order is providers.json (provider-name) order; sort_by_name
    reorders the model rows by display name without writing anything."""
    providers = [
        p for p in providers_doc.get("providers", [])
        if isinstance(p, dict) and p.get("id")
    ]
    lines: list = []
    model_rows: list = []
    for provider in providers:
        pid = provider["id"]
        penabled = bool(provider.get("enabled", True))
        mm = provider.get("models")
        if not isinstance(mm, dict):
            continue
        pname = provider.get("name") or pid
        for mid, m in mm.items():
            if not isinstance(m, dict) or not m.get("enabled", True):
                continue
            if not penabled:
                continue
            mname = m.get("name") or mid
            model_rows.append((mname, pname, pid, mid))
    if sort_by_name:
        model_rows.sort(key=lambda r: (r[0].lower(), r[1].lower(), r[2], r[3]))
    total_enabled = len(model_rows)
    # First element is a heading marker: ("heading", text) -> drawn as a
    # full-width blue bar, like the screen title. Count sits on the bar
    # so paging cannot park a second "Summary" line on the status row.
    lines.append(("heading", f"Enabled Models: {total_enabled}"))
    lines.append([("", P.TEXT)])  # gap under the models header
    for mname, pname, pid, mid in model_rows:
        level = _model_reasoning_level(_model_entry(providers, pid, mid))
        level_pair = P.FREE if level != "none" else P.MUTED
        lines.append(("model", pid, mid, [
            ("● ", P.ENABLED),
            (mname, P.VALUE),
            (f" ({pname}) ", P.TEXT),
            (f"({level})", level_pair),
        ]))
    if not total_enabled:
        lines.append([("No enabled models. Enable with --enable or grok-models", P.MUTED)])
        return lines
    return lines


def cmd_disable_all() -> int:
    providers_doc = load_providers()
    changed = False
    for provider in providers_doc.get("providers", []):
        if not isinstance(provider, dict):
            continue
        models = provider.get("models")
        if not isinstance(models, dict):
            continue
        for mid, m in models.items():
            if isinstance(m, dict) and m.get("enabled", True):
                m["enabled"] = False
                changed = True
    if not changed:
        print("All models already disabled.")
        return 0
    dump_providers(PROVIDERS_PATH, providers_doc)
    path, stats = run_sync()
    if path is not None:
        print_sync_report(stats, path, providers_doc)
        print_relaunch()
    return 0


def resolve_targets(
    providers_doc: dict, targets: list[str], want_enabled: bool
) -> list[tuple[dict, str | None]]:
    """Resolve 'provider' or 'provider/model' targets against providers.json.

    Model ids are normalized ('.', '/', ':' -> '_') on both sides so TOML-style
    keys also match. Returns (provider_dict, model_id_or_None) pairs; raises
    SyncError with close-match hints for anything unresolvable.
    """
    def norm(s: str) -> str:
        return s.replace(".", "_").replace("/", "_").replace(":", "_")

    providers = [
        p for p in providers_doc.get("providers", [])
        if isinstance(p, dict) and p.get("id")
    ]
    resolved: list[tuple[dict, str | None]] = []
    errors: list[str] = []
    for target in targets:
        pid, sep, mid = target.partition("/")
        matches = [p for p in providers if norm(p["id"]) == norm(pid)]
        if len(matches) != 1:
            hints = difflib.get_close_matches(pid, [p["id"] for p in providers])
            hint = f" (did you mean: {', '.join(hints)}?)" if hints else ""
            errors.append(f"unknown provider {pid!r}{hint}")
            continue
        provider = matches[0]
        if not sep:
            resolved.append((provider, None))
            continue
        raw_ids = list(provider.get("models", {}).keys())
        model_hits = [mid0 for mid0 in raw_ids if norm(mid0) == norm(mid)]
        if len(model_hits) != 1:
            hints = difflib.get_close_matches(mid, raw_ids)
            hint = f" (did you mean: {', '.join(hints)}?)" if hints else ""
            errors.append(f"unknown model {mid!r} for provider {pid!r}{hint}")
            continue
        resolved.append((provider, model_hits[0]))
    if errors:
        fail("cannot apply: " + "; ".join(errors))
    return resolved


def _record_removed_provider(
    providers_doc: dict, pid: str, models: list[str] | None = None
) -> None:
    """Record a deleted provider with its enabled model ids so sync can
    remove its config.toml tables by exact key."""
    removed = providers_doc.setdefault("removed_providers", [])
    already = any(
        (e.get("provider") == pid if isinstance(e, dict) else e == pid)
        for e in removed
    )
    if not already:
        removed.append({"provider": pid, "models": models or []})


def enabled_model_ids(provider: dict) -> list[str]:
    """The enabled model ids of a provider entry from providers.json."""
    out: list[str] = []
    models = provider.get("models")
    if isinstance(models, dict):
        for mid, m in models.items():
            if bool(m.get("enabled", True)) if isinstance(m, dict) else True:
                out.append(mid)
    return out


def cmd_toggle(enable_targets: list[str], disable_targets: list[str]) -> int:
    providers_doc = load_providers()
    # A 'provider/model' enable target whose provider was never added used to
    # die in resolve_targets with "unknown provider". Add the provider first
    # (all models disabled), then the resolution below flips just that model.
    existing_ids = {
        p.get("id") for p in providers_doc["providers"] if isinstance(p, dict)
    }
    missing_providers: list[str] = []
    for target in enable_targets:
        pid, sep, _ = target.partition("/")
        if sep and pid not in existing_ids and pid not in missing_providers:
            missing_providers.append(pid)
    if missing_providers:
        api = fetch_models_dev()
        for pid in missing_providers:
            add_provider_entry(providers_doc, api, pid)

    resolved_enable = resolve_targets(providers_doc, enable_targets, True)
    resolved_disable = resolve_targets(providers_doc, disable_targets, False)

    # Later flags win when both lists hit the same target.
    applied: dict[tuple[int, str | None], tuple[dict, str | None, bool]] = {}
    for provider, mid, want in (
        [(p, m, True) for p, m in resolved_enable]
        + [(p, m, False) for p, m in resolved_disable]
    ):
        key = (id(provider), mid)
        applied[key] = (provider, mid, want)

    disabled_provider_ids = {
        p["id"]
        for p, m, want in applied.values()
        if m is not None and want and not p.get("enabled", True)
    }

    changed = False
    for (provider, mid, want) in applied.values():
        label = provider["id"] + (f"/{mid}" if mid else "")
        if mid is None:
            if provider.get("enabled", True) == want:
                print(f"already {'enabled' if want else 'disabled'}: {label}")
                continue
            provider["enabled"] = want
            print(f"{'enabled' if want else 'disabled'}: {label}")
        else:
            models = provider.setdefault("models", {})
            m = models.get(mid)
            if not isinstance(m, dict):
                m = models[mid] = {}
            if m.get("enabled", True) == want:
                print(f"already {'enabled' if want else 'disabled'}: {label}")
                continue
            m["enabled"] = want
            print(f"{'enabled' if want else 'disabled'}: {label}")
        changed = True

    if not changed:
        return 0

    dump_providers(PROVIDERS_PATH, providers_doc)
    for pid in sorted(disabled_provider_ids):
        print(
            f"warning: provider {pid!r} is disabled; enable it too or its "
            f"models won't be written to config.toml"
        )
    path, stats = run_sync()
    if path is not None:
        print_sync_report(stats, path, providers_doc)
        print_relaunch()
    return 0


def context_window_field(minfo: dict) -> int | None:
    """models.dev limit.context as an int (bools excluded, floats truncated)."""
    limit = minfo.get("limit")
    if not isinstance(limit, dict):
        return None
    context = limit.get("context")
    if isinstance(context, (int, float)) and not isinstance(context, bool):
        return int(context)
    return None


def enrich_model_entry(
    entry: dict, minfo: dict, provider_id: str, provider_npm: str | None
) -> bool:
    """Fill a model's missing attributes (context window, reasoning effort
    options) from its models.dev catalog entry. Existing values are never
    overwritten — user-set preferences win. Catalog `modalities` and `npm`
    are refreshed whenever the catalog carries them."""
    added = False
    npm = catalog_npm(minfo.get("provider"))
    if npm:
        entry["npm"] = npm
    write_api_backend(entry, provider_id, provider_npm)
    mods = catalog_modalities(minfo)
    if mods is not None:
        entry["modalities"] = mods
    if "context_window" not in entry:
        ctx = context_window_field(minfo)
        if ctx is not None:
            entry["context_window"] = ctx
    if minfo.get("reasoning"):
        efforts = efforts_from_models_dev(minfo)
        if efforts:
            default_row = next(
                (row for row in efforts if row.get("default")), efforts[0]
            )
            for key, value in (
                ("supports_reasoning_effort", True),
                ("reasoning_efforts", efforts),
                ("reasoning_effort", default_row["value"]),
            ):
                if key not in entry:
                    entry[key] = value
        elif "supports_reasoning_effort" not in entry:
            entry["supports_reasoning_effort"] = True



def efforts_from_models_dev(minfo: dict) -> list[dict] | None:
    options = minfo.get("reasoning_options") or []
    if not isinstance(options, list):
        return None
    values: list[str] = []
    for opt in options:
        if isinstance(opt, dict) and opt.get("type") == "effort":
            values = opt.get("values") or []
            break
    if not values:
        return None
    rows = []
    for val in values:
        val = str(val)
        rows.append(
            {
                "id": val,
                "value": val,
                "label": f"{first_letter_cap(val)} Effort",
                "default": False,
            }
        )
    if not rows:
        return None
    default_row = next((row for row in rows if row["value"] != "none"), rows[0])
    default_row["default"] = True
    return rows


def build_fields(
    model_id: str,
    minfo: dict,
    base_url: str,
    env_key: str,
    provider_name: str,
    stored_name: str | None = None,
    include_descriptions: bool = INCLUDE_DESCRIPTIONS_DEFAULT,
) -> dict:
    """Map a models.dev model entry to Grok Build [model.*] TOML fields."""
    left = (
        stored_name
        if isinstance(stored_name, str) and stored_name
        else (minfo.get("name") or first_letter_cap(model_id))
    )
    fields: dict = {
        "model": model_id,
        "base_url": base_url,
        "name": f"{left} ({provider_name})",
        "env_key": env_key,
        "api_backend": "chat_completions",
    }
    limit = minfo.get("limit")
    if isinstance(limit, dict):
        context = limit.get("context")
        if isinstance(context, (int, float)) and not isinstance(context, bool):
            fields["context_window"] = int(context)
    if minfo.get("reasoning"):
        efforts = efforts_from_models_dev(minfo)
        if efforts:
            fields["supports_reasoning_effort"] = True
            fields["reasoning_efforts"] = efforts
            default_row = next((row for row in efforts if row.get("default")), efforts[0])
            fields["reasoning_effort"] = default_row["value"]
        else:
            fields["supports_reasoning_effort"] = True
    desc = catalog_description(minfo)
    if include_descriptions and isinstance(desc, str) and desc:
        fields["description"] = desc
    return fields


def is_table_header(line: str) -> bool:
    stripped = line.lstrip()
    return stripped.startswith("[") and "]" in stripped


def owned_table_key(header: str) -> str | None:
    inner = header.strip()
    if inner.startswith("[[") and inner.endswith("]]"):
        inner = inner[2:-2]
    elif inner.startswith("[") and inner.endswith("]"):
        inner = inner[1:-1]
    else:
        return None
    inner = inner.strip()
    if not inner.startswith("model."):
        return None
    return inner[6:].split(".", 1)[0]


def is_owned_header(header: str, provider_ids: list[str]) -> bool:
    key = owned_table_key(header)
    if not key:
        return False
    return any(key.startswith(pid + "-") for pid in provider_ids)


def strip_removed_and_unowned_sections(
    text: str, provider_ids: list[str], removed_keys: set[str]
) -> str:
    if not text:
        return ""
    lines = text.splitlines(keepends=True)
    out: list[str] = []
    i = 0
    while i < len(lines):
        if is_table_header(lines[i]):
            key = owned_table_key(lines[i])
            # Exact-key match for recorded deletions; prefix match only for
            # the tool's own rebuildable tables.
            is_removed = key is not None and key in removed_keys
            is_owned = is_owned_header(lines[i], provider_ids)
            if is_removed or is_owned:
                i += 1
                while i < len(lines) and not is_table_header(lines[i]):
                    i += 1
                continue
        out.append(lines[i])
        i += 1
    return "".join(out)


def toml_escape(value: object) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int) and not isinstance(value, bool):
        return str(value)
    if isinstance(value, str):
        escaped = value.replace("\\", "\\\\").replace('"', '\\"')
        return f'"{escaped}"'
    fail(f"unsupported TOML value type: {type(value).__name__}")


def emit_model_table(table_key: str, fields: dict) -> str:
    lines = [f"[model.{table_key}]"]
    for key in TOML_SCALAR_FIELDS:
        if key == "api_backend":
            lines.append(f"{key} = {toml_escape(fields.get(key) or 'chat_completions')}")
            continue
        if key in OPTIONAL_META_FIELDS[0:3]:
            if key in fields:
                lines.append(f"{key} = {toml_escape(fields[key])}")
            continue
        if key in fields:
            lines.append(f"{key} = {toml_escape(fields[key])}")
    efforts = fields.get("reasoning_efforts") or []
    for row in efforts:
        lines.append("")
        lines.append(f"[[model.{table_key}.reasoning_efforts]]")
        for rk in ("id", "value", "label", "default"):
            lines.append(f"{rk} = {toml_escape(row[rk])}")
    return "\n".join(lines) + "\n"


def write_toml_stdlib(
    path: Path,
    provider_ids: list[str],
    tables: list[tuple[str, dict]],
    removed_keys: set[str] | None = None,
) -> str:
    removed_keys = removed_keys or set()
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    kept = strip_removed_and_unowned_sections(existing, provider_ids, removed_keys)
    chunks = [kept.rstrip()]
    for table_key, fields in tables:
        chunks.append(emit_model_table(table_key, fields).rstrip())
    text = "\n\n".join(chunk for chunk in chunks if chunk) + "\n"
    return text


def validate_toml_text(text: str) -> None:
    try:
        import tomllib
    except ImportError:
        return
    try:
        tomllib.loads(text)
    except Exception as exc:
        fail(f"invalid TOML write: {exc}")


def write_config_toml(
    provider_ids: list[str],
    tables: list[tuple[str, dict]],
    removed_keys: set[str] | None = None,
) -> Path:
    path = CONFIG_TOML_PATH
    if path.exists():
        shutil.copy2(path, path.with_name(path.name + ".bak"))
    text = write_toml_stdlib(path, provider_ids, tables, removed_keys)
    validate_toml_text(text)
    try:
        atomic_write(path, text)
    except OSError as exc:
        fail(f"failed to write {path}: {exc}")
    return path


def _pid_for_table_key(table_key: str, provider_ids: list[str]) -> str | None:
    matches = [
        pid
        for pid in provider_ids
        if table_key == pid or table_key.startswith(pid + "-")
    ]
    if not matches:
        return None
    return max(matches, key=len)


def _toml_key(ident: str) -> str:
    if re.fullmatch(r"[A-Za-z0-9_-]+", ident):
        return ident
    return toml_escape(ident)


def emit_codex_provider_table(pid: str, fields: dict) -> str:
    name = fields.get("name") or pid
    base_url = fields.get("base_url") or ""
    env_key = fields.get("env_key") or ""
    wire_api = "responses"
    lines = [
        f"[model_providers.{_toml_key(pid)}]",
        f"name = {toml_escape(name)}",
        f"base_url = {toml_escape(base_url)}",
        f"env_key = {toml_escape(env_key)}",
        f"wire_api = {toml_escape(wire_api)}",
    ]
    return "\n".join(lines) + "\n"


def _is_codex_managed_key(stripped: str) -> bool:
    return (
        stripped.startswith("model =")
        or stripped.startswith("model_provider =")
        or stripped.startswith("model_catalog_json =")
    )


def _codex_owned_provider_ids(doc: dict, extra_pid: str = "") -> list[str]:
    """Provider ids this tool may rewrite under [model_providers.*].

    Configured ids from providers.json, plus the remembered Codex provider
    (so a just-deleted selection can still be stripped). Not removed_providers.
    """
    seen: set[str] = set()
    out: list[str] = []
    for p in doc.get("providers") or []:
        if not isinstance(p, dict):
            continue
        pid = p.get("id")
        if isinstance(pid, str) and pid and pid not in seen:
            seen.add(pid)
            out.append(pid)
    if extra_pid and extra_pid not in seen:
        out.append(extra_pid)
    return out


def _strip_codex_managed_sections(text: str, provider_ids: list[str]) -> str:
    """Drop this tool's root Codex keys and owned [model_providers.<id>] tables.

    Root `model` / `model_provider` / `model_catalog_json` (before the first
    table) and owned provider tables are removed. [projects], [profiles],
    and any other tables are left untouched.
    """
    if not text:
        return ""
    owned = set(provider_ids)
    lines = text.splitlines(keepends=True)
    out: list[str] = []
    i = 0
    in_root = True
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_root = False
            header = stripped[1:-1].strip()
            pid = None
            if header.startswith("model_providers."):
                rest = header.split(".", 1)[1]
                pid = rest.strip().strip('"')
            if pid is not None and pid in owned:
                i += 1
                while i < len(lines):
                    nxt = lines[i].strip()
                    if nxt.startswith("[") and nxt.endswith("]"):
                        break
                    i += 1
                continue
        elif in_root and _is_codex_managed_key(stripped):
            i += 1
            continue
        out.append(line)
        i += 1
    return "".join(out)


def _codex_catalog_reasoning_levels(entry: dict) -> tuple[list[dict], str | None]:
    levels: list[dict] = []
    default = None
    efforts = entry.get("reasoning_efforts")
    if isinstance(efforts, list):
        for item in efforts:
            if not isinstance(item, dict):
                continue
            effort = item.get("value") or item.get("effort")
            if not isinstance(effort, str) or not effort:
                continue
            desc = item.get("label") or item.get("description") or effort
            levels.append({"effort": effort, "description": str(desc)})
            if item.get("default"):
                default = effort
    stored = entry.get("reasoning_effort")
    if isinstance(stored, str) and stored:
        default = stored
    return levels, default


CODEX_INPUT_MODALITY_VALUES = ("text", "image", "audio")


def codex_input_modalities(entry: dict) -> list[str]:
    """Codex-allowed input modalities from a stored providers.json model."""
    modalities = entry.get("modalities")
    if not isinstance(modalities, dict):
        return []
    raw = modalities.get("input")
    if not isinstance(raw, list):
        return []
    out: list[str] = []
    seen: set[str] = set()
    for item in raw:
        if item in CODEX_INPUT_MODALITY_VALUES and item not in seen:
            out.append(item)
            seen.add(item)
    return out


def emit_codex_model_catalog(provider: dict) -> dict:
    """Codex `model_catalog_json` payload for one provider's enabled models."""
    models_out: list[dict] = []
    models = provider.get("models")
    if not isinstance(models, dict):
        models = {}
    for i, (mid, m) in enumerate(models.items()):
        entry = m if isinstance(m, dict) else {}
        if not bool(entry.get("enabled", True)):
            continue
        name = entry.get("name")
        display = name if isinstance(name, str) and name else mid
        desc = entry.get("description")
        description = desc if isinstance(desc, str) else ""
        ctx = entry.get("context_window")
        try:
            context_window = int(ctx) if ctx is not None else 128000
        except (TypeError, ValueError):
            context_window = 128000
        levels, default_level = _codex_catalog_reasoning_levels(entry)
        item: dict = {
            "slug": mid,
            "display_name": display,
            "description": description,
            "context_window": context_window,
            "max_context_window": context_window,
            "supported_reasoning_levels": levels,
            "shell_type": "shell_command",
            "visibility": "list",
            "supported_in_api": True,
            "priority": i,
            "base_instructions": "",
            "supports_reasoning_summaries": bool(entry.get("supports_reasoning_effort")),
            "default_reasoning_summary": "none",
            "support_verbosity": False,
            "truncation_policy": {"mode": "tokens", "limit": 10000},
            "effective_context_window_percent": 95,
            "experimental_supported_tools": [],
        }
        input_modalities = codex_input_modalities(entry)
        if input_modalities:
            item["input_modalities"] = input_modalities
        if default_level:
            item["default_reasoning_level"] = default_level
        models_out.append(item)
    return {"models": models_out}


def write_codex_model_catalog(provider_id: str, provider: dict) -> Path:
    path = codex_models_json_path(provider_id)
    payload = emit_codex_model_catalog(provider)
    try:
        atomic_write(path, json.dumps(payload, indent=2, ensure_ascii=False) + "\n")
    except OSError as exc:
        fail(f"failed to write {path}: {exc}")
    return path


def remove_codex_model_catalog(provider_id: str) -> None:
    if not provider_id:
        return
    path = codex_models_json_path(provider_id)
    try:
        path.unlink()
    except FileNotFoundError:
        pass
    except OSError as exc:
        fail(f"failed to remove {path}: {exc}")


def _codex_provider_fields(provider: dict, pid: str) -> dict:
    pname = provider.get("name") or pid
    return {
        "name": pname,
        "base_url": provider.get("base_url") if isinstance(provider.get("base_url"), str) else "",
        "env_key": first_env_key(provider),
    }


def codex_config_toml(
    providers_doc: dict,
    provider_ids: list[str] | None = None,
    tables: list[tuple[str, dict]] | None = None,
    removed_keys: set[str] | None = None,
) -> Path:
    """Sibling of write_config_toml: emit one Codex provider block at the
    top of $CODEX_HOME/config.toml, plus the matching model catalog JSON.

    Called when write is on, or once after disable/delete while
    `codex_model_provider` is still set. That field is the Codex-side
    memory of which table to clear; removed_providers is Grok-only.
    Disable/delete clears the field, strips this tool's root keys and the
    remembered [model_providers.<id>] table, deletes <id>-models.json,
    and does not write those keys back.
    """
    del provider_ids, tables, removed_keys
    flag = bool(
        providers_doc.get("write_codex_config_toml", WRITE_CODEX_CONFIG_TOML_DEFAULT)
    )
    pid = codex_model_provider_id(providers_doc)
    remembered = pid
    # One-shot cleanup after disable or delete of the Codex provider:
    # drop the remembered provider, then strip the old block.
    if not flag and pid:
        providers_doc["codex_model_provider"] = ""
        dump_providers(PROVIDERS_PATH, providers_doc)
        pid = ""
    owned = _codex_owned_provider_ids(providers_doc, remembered)
    path = codex_config_toml_path()
    provider = find_provider(providers_doc, pid) if pid else None
    first_mid = first_enabled_model_id(provider) if provider else None
    should_emit = bool(flag and provider and first_mid)

    if not should_emit:
        remove_codex_model_catalog(remembered)

    if not path.exists() and not should_emit:
        return path

    if path.exists():
        shutil.copy2(path, path.with_name(path.name + ".bak"))
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    kept = _strip_codex_managed_sections(existing, owned).strip("\n")

    prefix = ""
    if should_emit:
        write_codex_model_catalog(pid, provider)
        catalog = codex_models_json_toml_value(pid)
        fields = _codex_provider_fields(provider, pid)
        prefix = (
            f"model = {toml_escape(first_mid)}\n"
            f"model_provider = {toml_escape(pid)}\n"
            f"model_catalog_json = {toml_escape(catalog)}\n"
            f"\n"
            f"{emit_codex_provider_table(pid, fields).rstrip()}"
        )

    if prefix and kept:
        text = f"{prefix}\n\n{kept}\n"
    elif prefix:
        text = f"{prefix}\n"
    elif kept:
        text = f"{kept}\n"
    else:
        text = ""
    if text:
        validate_toml_text(text)
    try:
        atomic_write(path, text)
    except OSError as exc:
        fail(f"failed to write {path}: {exc}")
    return path


def update_providers_json(*, quiet: bool = False) -> dict:
    """Update phase (1 of 2): reconcile every configured provider's model list
    in providers.json against fresh data (live /models with catalog fallback)
    and backfill env_key/npm/base_url. Fetches models.dev itself. Reads and
    writes only providers.json — no config.toml involvement."""
    models_dev = fetch_models_dev()
    providers_doc = load_providers()
    stats = {
        "providers_synced": 0,
        "models_added": 0,
        "models_removed": 0,
        "models_renamed": 0,
        "descriptions_updated": 0,
        "models_missing": 0,
        "providers_missing": 0,
        "tables_written": 0,
        "live_fetch_errors": [],
    }

    # Refresh every configured provider, enabled or not — a disabled
    # provider's stored model list must stay current so re-enabling it
    # doesn't surface stale data. (Only enabled providers reach config.toml;
    # that filter lives in update_config_toml.)
    for provider in providers_doc["providers"]:
        if not isinstance(provider, dict) or not provider.get("id"):
            continue
        pid = provider["id"]
        pinfo = models_dev.get(pid)
        if not isinstance(pinfo, dict):
            if not quiet:
                print(f"  warning: provider {pid!r} not found in models.dev; skipping")
            stats["providers_missing"] += 1
            continue
        catalog_models = catalog_models_dict(pinfo)

        new_env_key = api_env_key(pinfo)
        if new_env_key and provider.get("env_key") != new_env_key:
            provider["env_key"] = new_env_key
        npm = catalog_npm(pinfo)
        if npm:
            provider["npm"] = npm

        models_map = provider.get("models")
        if not isinstance(models_map, dict):
            models_map = {}
            provider["models"] = models_map

        # A stored non-empty base_url wins over the catalog; missing/empty
        # backfills from the catalog. /models is fetched from this stored URL.
        stored = provider.get("base_url")
        if not isinstance(stored, str):
            stored = ""
        catalog_api = pinfo.get("api") or ""
        if not stored and catalog_api:
            provider["base_url"] = catalog_api
        base_url = stored or ""

        items, err = authority_items_for_provider(
            pinfo,
            base_url,
            quiet=quiet,
            env_key=first_env_key(provider),
            provider=provider,
        )
        if err:
            stats["live_fetch_errors"].append(err)
        reconcile_models_map(
            models_map,
            items,
            catalog_models,
            stats,
            pid,
            catalog_npm(pinfo),
        )
        stats["providers_synced"] += 1

    providers_doc["last_updated"] = last_updated_stamp()
    dump_providers(PROVIDERS_PATH, providers_doc)
    return stats


def update_config_toml(*, quiet: bool = False) -> Path:
    """Write phase (2 of 2): load providers.json from disk and render
    config.toml from it alone — enabled providers, table fields, table
    ownership, and pending deletions are all derived from the file."""
    providers_doc = load_providers()

    managed = {
        p["id"]
        for p in providers_doc["providers"]
        if isinstance(p, dict) and p.get("id")
    }

    removed_entries = providers_doc.get("removed_providers", [])
    removed_keys: set[str] = set()
    for entry in removed_entries:
        # Only entries carrying explicit provider+model ids participate;
        # nothing is ever removed by provider id alone.
        if not isinstance(entry, dict):
            continue
        pid = entry.get("provider")
        models = entry.get("models")
        if not isinstance(pid, str) or not isinstance(models, list):
            continue
        managed.add(pid)
        for mid in models:
            if isinstance(mid, str):
                removed_keys.add(table_model_id(pid, mid))

    include_descriptions = bool(
        providers_doc.get("include_descriptions", INCLUDE_DESCRIPTIONS_DEFAULT)
    )

    tables: list[tuple[str, dict]] = []
    for provider in providers_doc["providers"]:
        if not isinstance(provider, dict) or not provider.get("id"):
            continue
        if not bool(provider.get("enabled", True)):
            continue
        pid = provider["id"]
        # base_url comes straight from providers.json; empty means the
        # provider has none stored and the catalog had nothing to backfill.
        base_url = (
            provider.get("base_url")
            if isinstance(provider.get("base_url"), str)
            else ""
        )
        if not base_url and not quiet:
            print(
                f"  warning: provider {pid!r} has no base URL; "
                f"tables will have an empty base_url"
            )
        env_key = first_env_key(provider)
        pname = provider.get("name") or pid

        models = provider.get("models")
        for mid, m in (models or {}).items():
            entry = m if isinstance(m, dict) else {}
            if not bool(entry.get("enabled", True)):
                continue
            # Assemble the table fields from stored values only.
            name = entry.get("name")
            if not isinstance(name, str) or not name:
                name = first_letter_cap(mid)
            backend = entry.get("api_backend")
            if not isinstance(backend, str) or not backend:
                backend = "chat_completions"
            fields: dict = {
                "model": mid,
                "base_url": base_url,
                "name": f"{name} ({pname})",
                "env_key": env_key,
                "api_backend": backend,
            }
            if "context_window" in entry:
                fields["context_window"] = entry["context_window"]
            if entry.get("supports_reasoning_effort"):
                fields["supports_reasoning_effort"] = True
                if "reasoning_efforts" in entry:
                    fields["reasoning_efforts"] = entry["reasoning_efforts"]
                    if "reasoning_effort" in entry:
                        fields["reasoning_effort"] = entry["reasoning_effort"]
            desc = entry.get("description")
            if include_descriptions and isinstance(desc, str) and desc:
                fields["description"] = desc
            tables.append((table_model_id(pid, mid), fields))

    path = write_config_toml(managed, tables, removed_keys)
    if reset_codex_if_invalid(providers_doc):
        dump_providers(PROVIDERS_PATH, providers_doc)
    flag = bool(
        providers_doc.get("write_codex_config_toml", WRITE_CODEX_CONFIG_TOML_DEFAULT)
    )
    if flag or codex_model_provider_id(providers_doc):
        codex_config_toml(providers_doc, list(managed), tables, removed_keys)

    # The deletion list has been consumed; clear it so it isn't reprocessed
    # forever, and persist that.
    if removed_entries:
        providers_doc["removed_providers"] = []
    providers_doc["last_synced"] = last_updated_stamp()
    dump_providers(PROVIDERS_PATH, providers_doc)
    return path


def run_sync() -> tuple[Path | None, dict]:
    """Reconcile providers.json with the live API, then rewrite
    ~/.grok/config.toml from it."""
    # Phase 1: update the models in providers.json.
    stats = update_providers_json()

    # Phase 2: rewrite config.toml from providers.json.
    path = update_config_toml()
    return path, stats




def print_sync_report(stats: dict, path: Path, providers_doc: dict) -> None:
    """Sync summary followed by required env vars for enabled providers."""
    print_summary(stats, path)
    print_env_requirements(providers_doc)


def add_provider_entry(
    providers_doc: dict, models_dev: dict, provider_id: str, quiet: bool = False
) -> str | None:
    """Add provider_id to providers_doc with all models disabled and persist.
    quiet suppresses stdout reports — required when called inside the curses
    TUI, where any raw print corrupts the screen.
    Returns the live /models URL if that fetch failed (catalog fallback used),
    otherwise None."""
    existing = {p.get("id") for p in providers_doc["providers"] if isinstance(p, dict)}
    if provider_id in existing:
        if not quiet:
            print(f"Provider {provider_id!r} already exists.")
        return None
    pinfo = models_dev.get(provider_id)
    if not isinstance(pinfo, dict):
        fail(f"provider {provider_id!r} not found in models.dev")
    catalog_models = catalog_models_dict(pinfo)

    entry = {
        "id": provider_id,
        "name": pinfo.get("name") or provider_id,
    }
    env = api_env_key(pinfo)
    if env:
        entry["env_key"] = env
    npm = catalog_npm(pinfo)
    if npm:
        entry["npm"] = npm
    api_base = pinfo.get("api")
    if isinstance(api_base, str) and api_base:
        entry["base_url"] = api_base
    base_url = entry.get("base_url") if isinstance(entry.get("base_url"), str) else ""
    items, fetch_err_url = authority_items_for_provider(
        pinfo, base_url, quiet=quiet, env_key=env, provider=entry
    )
    if not items:
        fail(f"provider {provider_id!r} has no models in models.dev")
    models_map = seed_models_from_items(
        items, catalog_models, provider_id, catalog_npm(pinfo)
    )
    entry["enabled"] = True
    entry["models"] = models_map
    providers_doc["providers"].append(entry)
    dump_providers(PROVIDERS_PATH, providers_doc)
    if not quiet:
        print(f"Added provider {provider_id!r} with {len(models_map)} models (all disabled).")
    return fetch_err_url


def cmd_search(term: str) -> int:
    """Search the models.dev provider list; the selected provider is added."""
    api = fetch_models_dev()
    provider_id = search_providers(api, term)
    if provider_id is None:
        return 0
    providers_doc = load_providers()
    add_provider_entry(providers_doc, api, provider_id)
    return 0


def cmd_add_provider(provider_id: str) -> int:
    providers_doc = load_providers()
    api = fetch_models_dev()
    add_provider_entry(providers_doc, api, provider_id)
    return 0


def cmd_codex(raw: str) -> int:
    providers_doc = load_providers()
    pid = raw.strip()
    if pid == "disabled":
        set_codex_selection(providers_doc, None)
        dump_providers(PROVIDERS_PATH, providers_doc)
        print("Codex Config disabled")
        return 0
    if pid not in enabled_provider_ids(providers_doc):
        fail(f"--codex requires 'disabled' or an enabled provider id (got {pid!r})")
    set_codex_selection(providers_doc, pid)
    dump_providers(PROVIDERS_PATH, providers_doc)
    print(f"Codex Config {pid}")
    return 0


def cmd_import() -> int:
    """Seed providers.json from the [model.*] tables already in config.toml,
    then enable those models. Reuses --add-provider and --enable, so no custom
    reconcile code is needed."""
    import tomllib

    if not CONFIG_TOML_PATH.exists():
        print("No config.toml found; nothing to import.")
        return 0
    try:
        with open(CONFIG_TOML_PATH, "rb") as fh:
            toml_data = tomllib.load(fh)
    except Exception as exc:
        fail(f"failed to read {CONFIG_TOML_PATH}: {exc}")

    model_tables = toml_data.get("model")
    if not isinstance(model_tables, dict):
        print("No [model.*] tables in config.toml; nothing to import.")
        return 0

    provider_models: dict[str, list[str]] = {}
    for table_key, table in model_tables.items():
        if not isinstance(table, dict):
            continue
        model_id = table.get("model")
        if not isinstance(model_id, str):
            continue
        safe_model_id = (
            model_id.replace(".", "_").replace("/", "_").replace(":", "_")
        )
        provider_id = table_key.removesuffix("-" + safe_model_id)
        provider_models.setdefault(provider_id, []).append(model_id)

    if not provider_models:
        print("No [model.*] tables in config.toml; nothing to import.")
        return 0

    # add-provider no-ops on a provider id that already exists in
    # providers.json, so re-call it here for every imported provider and
    # capture which ids it skipped. Those skipped providers need an
    # explicit enable so the later run_sync reconciles them against the
    # models.dev catalog (adds missing models, drops dead ones) before
    # the per-model enables run.
    providers_doc_before_add = load_providers()
    existing_ids = {
        p.get("id")
        for p in providers_doc_before_add["providers"]
        if isinstance(p, dict)
    }
    enable_providers = [
        pid for pid in provider_models if pid in existing_ids
    ]

    for provider_id in provider_models:
        cmd_add_provider(provider_id)

    if enable_providers:
        cmd_toggle(enable_providers, [])

    enable_models = [
        f"{provider_id}/{model_id}"
        for provider_id, model_ids in provider_models.items()
        for model_id in model_ids
    ]
    disable_models: list[str] = []
    cmd_toggle(enable_models, disable_models)
    return 0


def _config_models(selected: dict, providers_doc: dict) -> bool:
    """Configure which models are enabled via a realtime substring search
    (numbered fallback; the curses path is handled inside the single-session
    flow). Returns True if any model's enabled state changed."""
    models = selected.get("models")
    if not isinstance(models, dict) or not models:
        print(f"No models for {selected['id']!r}. Run a sync or re-add the provider.")
        return False
    ids = list(models.keys())
    changed = _config_models_numbered(ids, models)
    if changed:
        dump_providers(PROVIDERS_PATH, providers_doc)
    enabled = sum(1 for mid in ids if models[mid].get("enabled", True))
    print(f"Updated models for {selected['id']!r}: {enabled} enabled of {len(ids)}.")
    return changed


def _confirm_delete(label: str) -> bool:
    while True:
        confirm = prompt_line(f"Delete Provider {label}? [no]", "no")
        parsed = parse_bool(confirm) if confirm else False
        if parsed is None:
            print("Enter yes or no.")
            continue
        return parsed


def _numbered_config_flow(providers_doc: dict, providers: list) -> bool:
    """Numbered (non-TTY) fallback for the entire TUI flow."""
    changed = False
    while True:
        # Order is providers.json (sorted only on dump).
        providers[:] = [
            p
            for p in providers_doc.get("providers", [])
            if isinstance(p, dict) and p.get("id")
        ]
        ordered = providers
        labels = [_provider_label(p) for p in ordered]
        pi = _numbered_select(labels, "Select a provider  (q quits)")
        if pi is None:
            break
        selected = ordered[pi]
        while True:
            enabled = bool(selected.get("enabled", True))
            actions = [
                "Configure Models",
                f"{'Disable' if enabled else 'Enable'} provider",
                "Delete Provider",
                "Back",
            ]
            env_line = _env_status_line(first_env_key(selected))
            footer = f"Required env var: {env_line}" if first_env_key(selected) else None
            ai = _numbered_select(
                actions,
                f"Provider: {selected.get('name') or selected['id']}  (1-4)",
                footer=footer,
            )
            if ai is None or actions[ai] == "Back":
                break
            if ai == 0:
                if _config_models(selected, providers_doc):
                    changed = True
            elif ai == 1:
                selected["enabled"] = not enabled
                dump_providers(PROVIDERS_PATH, providers_doc)
                verb = "Disabled" if enabled else "Enabled"
                print(f"{verb} provider {selected['id']!r}.")
                changed = True
            elif ai == 2:
                if _confirm_delete(_provider_display(selected)):
                    # Grab the enabled model ids from providers.json before
                    # the entry is removed, then flush the deletion
                    # immediately.
                    enabled = enabled_model_ids(selected)
                    providers_doc["providers"] = [
                        p
                        for p in providers_doc["providers"]
                        if p.get("id") != selected["id"]
                    ]
                    _record_removed_provider(providers_doc, selected["id"], enabled)
                    providers[:] = [
                        p for p in providers if p.get("id") != selected["id"]
                    ]
                    dump_providers(PROVIDERS_PATH, providers_doc)
                    update_config_toml()
                    print(f"Deleted Provider {_provider_display(selected)}.")
                    changed = True
                break
    return changed


def cmd_config() -> int:
    providers_doc = load_providers()
    providers = [p for p in providers_doc["providers"] if isinstance(p, dict) and p.get("id")]

    changed = False
    if sys.stdin.isatty() and sys.stdout.isatty():
        r = _curses_config_flow(providers_doc, providers)
        if r is _CURSES_FAILED:
            changed = _numbered_config_flow(providers_doc, providers)
        else:
            changed = bool(r)
    else:
        if not providers:
            print("No providers configured yet. Add with --add-provider")
            return 0
        changed = _numbered_config_flow(providers_doc, providers)

    if changed:
        path, stats = run_sync()
        if path is not None:
            print_sync_report(stats, path, providers_doc)
            print_relaunch()
    return 0


def print_summary(stats: dict, path: Path) -> None:
    print()
    print(f"Updated {path}")
    print("Sync Summary:")
    print(f"  providers synced: {stats.get('providers_synced', 0)}")
    print(f"  models added: {stats.get('models_added', 0)}")
    print(f"  models removed: {stats.get('models_removed', 0)}")
    print(f"  models renamed: {stats.get('models_renamed', 0)}")
    print(f"  descriptions updated: {stats.get('descriptions_updated', 0)}")
    print(f"  models missing (skipped): {stats.get('models_missing', 0)}")
    print(f"  providers missing (skipped): {stats.get('providers_missing', 0)}")
    print(f"  tables written: {stats.get('tables_written', 0)}")


def print_relaunch() -> None:
    print("Relaunch Grok Build for model changes")


def cmd_sync() -> int:
    providers_doc = load_providers()
    path, stats = run_sync()
    if path is None:
        return 0
    print_sync_report(stats, path, providers_doc)
    print_relaunch()
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Manage Grok Build [model.*] tables from models.dev.\n\n"
            "Writes [model.<provider-id>-<model-id>] into ~/.grok/config.toml "
            "(or $GROK_HOME). Matching tables are added, updated, or deleted on "
            "sync. Give custom models unique table names so they are not overwritten.\n\n"
            "No arguments opens the interactive TUI (numbered menus if stdout is not a TTY)."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "quick start:\n"
            "  grok-models.py --add-provider opencode-go\n"
            "  grok-models.py --enable opencode-go/glm-5.3\n"
            "  grok-models.py                              then: TUI, or just use the model\n"
            "\n"
            "examples:\n"
            "  grok-models.py                              interactive TUI\n"
            "  grok-models.py --providers                  list configured providers\n"
            "  grok-models.py --provider opencode-go       list models for a provider\n"
            "  grok-models.py --models                     list enabled models\n"
            "  grok-models.py --add-provider opencode-go   add a provider (models start disabled)\n"
            "  grok-models.py --search glm                 search models.dev and add a provider\n"
            "  grok-models.py --enable opencode-go/glm-5.3 enable a model\n"
            "  grok-models.py --disable opencode-go/glm-5.3\n"
            "  grok-models.py --disable-all\n"
            "  grok-models.py --codex openrouter           write Codex config for this provider on sync (or 'disabled')\n"
            "  grok-models.py --sync                       refresh from models.dev; rewrite config.toml\n"
            "  grok-models.py --import                     pull [model.*] from an existing config.toml\n"
        ),
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "--providers",
        action="store_true",
        help="List configured providers",
    )
    group.add_argument(
        "--provider",
        metavar="ID",
        help="List models for this provider",
    )
    group.add_argument(
        "--models",
        action="store_true",
        help="List enabled models",
    )
    group.add_argument(
        "--add-provider",
        metavar="ID",
        help="Add provider ID from models.dev",
    )
    group.add_argument(
        "--search",
        metavar="TERM",
        help="Search models.dev providers and add one",
    )
    group.add_argument(
        "--enable",
        action="append",
        metavar="TARGET",
        default=[],
        help="Enable provider or provider/model (repeatable)",
    )
    group.add_argument(
        "--disable",
        action="append",
        metavar="TARGET",
        default=[],
        help="Disable provider or provider/model (repeatable)",
    )
    group.add_argument(
        "--disable-all",
        action="store_true",
        help="Disable every model in every provider",
    )
    group.add_argument(
        "--codex",
        metavar="PROVIDER",
        help="Write Codex config for this enabled provider on sync (or 'disabled')",
    )
    group.add_argument(
        "--sync",
        action="store_true",
        help="Refresh providers.json from models.dev; rewrite config.toml",
    )
    group.add_argument(
        "--import",
        dest="import_flag",
        action="store_true",
        help="Import providers/models from existing config.toml [model.*]",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.add_provider:
            return cmd_add_provider(args.add_provider)
        if args.import_flag:
            return cmd_import()
        if args.search:
            return cmd_search(args.search)
        if args.providers:
            render_list_text(load_providers(), None, providers_only=True)
            return 0
        if args.provider is not None:
            render_list_text(
                load_providers(), args.provider, providers_only=False
            )
            return 0
        if args.models:
            return render_models_text()
        if args.disable_all:
            return cmd_disable_all()
        if args.enable or args.disable:
            return cmd_toggle(args.enable, args.disable)
        if args.codex is not None:
            return cmd_codex(args.codex)
        if args.sync:
            return cmd_sync()
        # Default (no args): straight into the TUI.
        return cmd_config()
    except SyncError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("Interrupted.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
