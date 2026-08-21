#!/usr/bin/env python3
"""Sync Grok Build [model.*] tables from models.dev.

Providers and their models are tracked in `providers.json`. Model metadata
(base URL, env key, context window, reasoning) is taken live from
https://models.dev/api.json so no separate model cache is needed.
"""

from __future__ import annotations

import argparse
import copy
import curses
import json
import shutil
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
PROVIDERS_PATH = HERE / "providers.json"
MODELS_TOML_PATH = HERE / "models.toml"
MODELS_DEV_URL = "https://models.dev/api.json"

TOML_SCALAR_FIELDS = (
    "model",
    "base_url",
    "name",
    "env_key",
    "api_backend",
    "supports_reasoning_effort",
    "reasoning_effort",
    "context_window",
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


def fail(message: str) -> None:
    raise SyncError(message)


def atomic_write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    tmp.replace(path)


def dump_json(path: Path, obj: object) -> None:
    atomic_write(path, json.dumps(obj, indent=2, ensure_ascii=False) + "\n")


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


def http_get_json(url: str) -> object:
    headers = {"User-Agent": "sync_models.py", "Accept": "application/json"}
    req = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            raw = resp.read()
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")[:300]
        fail(f"HTTP {exc.code} fetching {url}: {body}")
    except urllib.error.URLError as exc:
        fail(f"HTTP failure fetching {url}: {exc.reason}")
    except TimeoutError:
        fail(f"HTTP timeout fetching {url}")
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


def first_letter_cap(text: str) -> str:
    if not text:
        return text
    return text[0].upper() + text[1:]


def first_env_key(provider: dict) -> str:
    env = provider.get("env")
    if isinstance(env, list) and env:
        return env[0] or ""
    return ""


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


def search_providers(api: dict) -> str | None:
    """Interactively search the models.dev provider list; return a chosen id."""
    term = prompt_required("Search term")
    term_l = term.lower()
    matches: list[tuple[str, str]] = []
    for pid, pinfo in api.items():
        if not isinstance(pinfo, dict):
            continue
        name = pinfo.get("name") or ""
        if term_l in pid.lower() or term_l in name.lower():
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


def _curses_init_colors() -> None:
    """Initialize color pairs for the TUI theme."""
    try:
        curses.use_default_colors()
    except curses.error:
        pass
    if curses.has_colors():
        curses.start_color()
        # Theme: provider header, enabled=green, disabled=red, free=cyan, 
        # selected=yellow on blue, dim=gray, separator=blue, warning=yellow, error=red
        curses.init_pair(1, curses.COLOR_GREEN, -1)    # enabled
        curses.init_pair(2, curses.COLOR_RED, -1)      # disabled
        curses.init_pair(3, curses.COLOR_CYAN, -1)     # free models
        curses.init_pair(4, curses.COLOR_YELLOW, -1)   # highlight/accent
        curses.init_pair(5, curses.COLOR_BLUE, -1)     # provider header
        curses.init_pair(6, curses.COLOR_MAGENTA, -1)  # filter/query
        curses.init_pair(7, -1, curses.COLOR_BLUE)     # selected item
        curses.init_pair(8, curses.COLOR_WHITE, curses.COLOR_BLUE)  # selected bright
        curses.init_pair(9, curses.COLOR_WHITE, -1)    # normal text
        curses.init_pair(10, curses.COLOR_BLACK, curses.COLOR_WHITE)  # header bar
        curses.init_pair(11, curses.COLOR_YELLOW, -1)  # warning/pending
        curses.init_pair(12, curses.COLOR_RED, -1)     # error/delete


def _curses_draw_header(stdscr, text: str, color_pair: int = 10) -> None:
    """Draw a full-width header bar."""
    height, width = stdscr.getmaxyx()
    try:
        stdscr.addstr(0, 0, " " * (width - 1), curses.color_pair(color_pair))
        stdscr.addstr(0, 2, text[:width - 4], curses.color_pair(color_pair) | curses.A_BOLD)
    except curses.error:
        pass


def _curses_draw_status(stdscr, text: str, color_pair: int = 9) -> None:
    """Draw status line at bottom."""
    height, width = stdscr.getmaxyx()
    try:
        stdscr.addstr(height - 1, 0, " " * (width - 1), curses.A_DIM)
        stdscr.addstr(height - 1, 0, text[:width - 1], curses.color_pair(9) | curses.A_DIM)
    except curses.error:
        pass


def _curses_select_win(
    stdscr,
    options: list[str],
    title: str,
    multi: bool = False,
    preselected: list[int] | None = None,
    allow_cancel: bool = True,
) -> int | list[int] | None:
    """curses selector drawn into an existing stdscr with color theme."""
    if not options:
        return None
    curses.curs_set(0)
    _curses_init_colors()
    state = set(preselected or [])
    current = 0
    n = len(options)
    top = 0
    while True:
        stdscr.erase()
        height, width = stdscr.getmaxyx()
        safe_w = max(1, width - 1)
        
        # Header bar
        _curses_draw_header(stdscr, f"  {title}")
        
        list_top = 2
        list_h = max(1, height - list_top - 2)
        if current < top:
            top = current
        elif current >= top + list_h:
            top = current - list_h + 1
        
        for row in range(list_h):
            idx = top + row
            if idx >= n:
                break
            opt = options[idx]
            if multi:
                mark = "●" if idx in state else "○"
                line = f"  {mark}  {opt}"
            else:
                line = f"  ▸ {opt}"
            line = line[:width - 2]
            
            is_sel = (idx == current)
            try:
                if is_sel:
                    stdscr.addstr(2 + row, 0, line.ljust(width - 1), curses.color_pair(8))
                else:
                    stdscr.addstr(2 + row, 0, line, curses.color_pair(9))
            except curses.error:
                pass
        
        # Separator line
        sep_y = 2 + min(n, height - 4)
        try:
            stdscr.addstr(sep_y, 0, "─" * (width - 1), curses.color_pair(5) | curses.A_DIM)
        except curses.error:
            pass
        
        # Hint bar
        hint = "↑/↓: move  Enter: select"
        if multi:
            hint += "  Space: toggle"
        hint += "  q/ESC: cancel"
        _curses_draw_status(stdscr, hint)
        
        stdscr.refresh()
        ch = stdscr.getch()
        if ch == curses.KEY_RESIZE:
            continue
        if ch == curses.KEY_UP and current > 0:
            current -= 1
        elif ch == curses.KEY_DOWN and current < n - 1:
            current += 1
        elif multi and ch == ord(" "):
            if current in state:
                state.discard(current)
            else:
                state.add(current)
        elif ch in (curses.KEY_ENTER, 10, 13):
            return sorted(state) if multi else current
        elif allow_cancel and ch in (ord("q"), 27):
            return None


def _numbered_select(
    options: list[str], title: str | None = None, allow_cancel: bool = True
) -> int | None:
    """Numbered fallback menu. Returns the chosen index, or None to cancel."""
    if not options:
        return None
    if title:
        print(title)
    for i, opt in enumerate(options, 1):
        print(f"  {i}. {opt}")
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
    """Return model indices sorted: enabled first, then free models, then alphabetical.
    Returns (filtered_indices, enabled_count, free_disabled_count)."""
    def is_free(mid: str) -> bool:
        return "free" in mid.lower()
    def is_enabled(mid: str) -> bool:
        m = models.get(mid)
        return bool(m.get("enabled", True)) if isinstance(m, dict) else False
    def sort_key(idx: int):
        mid = ids[idx]
        if is_enabled(mid):
            return (0, 0 if is_free(mid) else 1, mid.lower())
        return (1, 0 if is_free(mid) else 1, mid.lower())
    base = [i for i in range(len(ids)) if filter_query is None or filter_query.lower() in ids[i].lower()]
    base.sort(key=sort_key)
    enabled_count = sum(1 for i in base if is_enabled(ids[i]))
    free_disabled_count = sum(1 for i in base[enabled_count:] if is_free(ids[i]))
    return base, enabled_count, free_disabled_count


def _curses_model_search_win(ids: list[str], models: dict, stdscr) -> bool:
    """curses search widget drawn into an existing stdscr: type to filter
    model ids live, arrow to move, Enter toggles the selected model's
    enabled state, q/ESC finishes. Left/Right arrows page.
    Mutates models in place. Returns True if any toggle happened, False otherwise."""
    curses.curs_set(0)
    _curses_init_colors()
    query = ""
    current = 0
    top = 0
    changed = False
    while True:
        filtered, enabled_count, free_disabled_count = _sort_model_indices(ids, models, query)
        if not filtered:
            current = 0
        elif current >= len(filtered):
            current = len(filtered) - 1
        stdscr.erase()
        height, width = stdscr.getmaxyx()
        safe_w = max(1, width - 1)
        
        # Header with filter
        _curses_draw_header(stdscr, f"  Configure models  |  Filter: {query}")
        
        list_top = 2
        list_h = max(1, height - list_top - 2)
        if current < top:
            top = current
        elif current >= top + list_h:
            top = current - list_h + 1
        
        if not filtered:
            try:
                stdscr.addstr(2, 0, "  (no matches)", curses.color_pair(11))
            except curses.error:
                pass
        
        for row in range(list_h):
            idx = top + row
            if idx >= len(filtered):
                break
            real_i = filtered[idx]
            mid = ids[real_i]
            m = models[mid]
            enabled = bool(m.get("enabled", True)) if isinstance(m, dict) else False
            is_free = "free" in mid.lower()
            
            mark = "●" if enabled else "○"
            free_tag = "  [free]" if is_free and not enabled else ""
            line = f"  {mark}  {mid}{free_tag}"
            line = line[:width - 2]
            
            is_sel = (idx == current)
            try:
                if is_sel:
                    stdscr.addstr(2 + row, 0, line.ljust(width - 1), curses.color_pair(8))
                else:
                    if enabled:
                        stdscr.addstr(2 + row, 0, line, curses.color_pair(1))
                    elif is_free:
                        stdscr.addstr(2 + row, 0, line, curses.color_pair(3))
                    else:
                        stdscr.addstr(2 + row, 0, line, curses.color_pair(9))
            except curses.error:
                pass
        
        # Separator after enabled
        sep_idx = enabled_count
        if 0 < enabled_count < len(filtered) and top <= sep_idx - 1 < top + list_h:
            y = 2 + sep_idx - top
            try:
                stdscr.addstr(y, 0, "─" * (width - 1), curses.color_pair(5) | curses.A_DIM)
            except curses.error:
                pass
        
        # Separator after free-disabled
        free_sep_idx = enabled_count + free_disabled_count
        if free_disabled_count > 0 and free_sep_idx < len(filtered) and top <= free_sep_idx - 1 < top + list_h:
            y = 2 + free_sep_idx - top
            try:
                stdscr.addstr(y, 0, "─" * (width - 1), curses.color_pair(3) | curses.A_DIM)
            except curses.error:
                pass
        
        hint = "↑/↓: move  ←/→: page  Enter: toggle  q/ESC: done  Backspace: clear filter"
        _curses_draw_status(stdscr, hint)
        
        stdscr.refresh()
        ch = stdscr.getch()
        if ch == curses.KEY_RESIZE:
            continue
        if ch in (ord("q"), 27):  # q or ESC -> done
            return changed
        if ch == curses.KEY_UP and current > 0:
            current -= 1
        elif ch == curses.KEY_DOWN and current < len(filtered) - 1:
            current += 1
        elif ch == curses.KEY_RIGHT:
            # Page down: jump one screen down
            if current + list_h < len(filtered):
                current = min(current + list_h, len(filtered) - 1)
        elif ch == curses.KEY_LEFT:
            # Page up: jump one screen up
            if current - list_h >= 0:
                current = max(current - list_h, 0)
        elif ch in (curses.KEY_BACKSPACE, 127, 8):
            query = query[:-1]
            current = 0
            top = 0
        elif ch in (curses.KEY_ENTER, 10, 13):
            if filtered:
                real_i = filtered[current]
                mid = ids[real_i]
                m = models[mid]
                if not isinstance(m, dict):
                    m = models[mid] = {}
                m["enabled"] = not m.get("enabled", True)
                changed = True
        elif 32 <= ch <= 126:
            query += chr(ch)
            current = 0
            top = 0


def _curses_confirm_win(stdscr, prompt: str) -> bool:
    """Yes/no prompt drawn into an existing stdscr with color."""
    curses.curs_set(0)
    _curses_init_colors()
    stdscr.erase()
    height, width = stdscr.getmaxyx()
    safe_w = max(1, width - 1)
    try:
        _curses_draw_header(stdscr, "  Confirm")
        stdscr.addstr(2, 2, prompt[:width - 4], curses.color_pair(9))
        stdscr.addstr(height - 2, 2, "  Y: Yes   N: No  (ESC: cancel)", curses.color_pair(11))
    except curses.error:
        pass
    stdscr.refresh()
    while True:
        ch = stdscr.getch()
        if ch in (ord("y"), ord("Y")):
            return True
        if ch in (ord("n"), ord("N"), 27):  # ESC cancels
            return False


def _curses_config_flow(providers_doc: dict, providers: list) -> bool | object:
    """Run the whole --config flow inside ONE curses session so there is no
    terminal-mode flash between menus. Returns True if providers.json
    changed, False if not, or _CURSES_FAILED on any curses error."""
    import curses

    def main(stdscr) -> bool:
        changed = False
        while True:
            labels = [_provider_label(p) for p in providers]
            pi = _curses_select_win(stdscr, labels, "Select a provider  (q cancels)")
            if pi is None:
                return changed
            selected = providers[pi]
            while True:
                enabled = bool(selected.get("enabled", True))
                actions = [
                    "Configure models",
                    "Disable provider" if enabled else "Enable provider",
                    "Delete provider",
                    "Back",
                ]
                ai = _curses_select_win(
                    stdscr, actions, f"Provider: {selected['id']}  (q cancels)"
                )
                if ai is None or actions[ai] == "Back":
                    break
                if ai == 0:
                    if _curses_model_search_win(
                        list(selected["models"].keys()), selected["models"], stdscr
                    ):
                        dump_json(PROVIDERS_PATH, providers_doc)
                        changed = True
                elif ai == 1:
                    selected["enabled"] = not enabled
                    dump_json(PROVIDERS_PATH, providers_doc)
                    changed = True
                elif ai == 2:
                    if _curses_confirm_win(stdscr, f"Delete provider {selected['id']!r}?"):
                        providers_doc["providers"] = [
                            p
                            for p in providers_doc["providers"]
                            if p.get("id") != selected["id"]
                        ]
                        dump_json(PROVIDERS_PATH, providers_doc)
                        changed = True
                        return changed
        return changed

    try:
        return curses.wrapper(main)
    except Exception:
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
    return f"{p['id']} ({p.get('name') or p['id']}) [{state}]"


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
) -> dict:
    """Map a models.dev model entry to Grok Build [model.*] TOML fields."""
    fields: dict = {
        "model": model_id,
        "base_url": base_url,
        "name": f"{minfo.get('name') or first_letter_cap(model_id)} ({provider_name})",
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


def strip_owned_toml_sections(text: str, provider_ids: list[str]) -> str:
    if not text:
        return ""
    lines = text.splitlines(keepends=True)
    out: list[str] = []
    i = 0
    while i < len(lines):
        if is_table_header(lines[i]) and is_owned_header(lines[i], provider_ids):
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


def write_toml_stdlib(path: Path, provider_ids: list[str], tables: list[tuple[str, dict]]) -> str:
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    kept = strip_owned_toml_sections(existing, provider_ids)
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


def write_models_toml(provider_ids: list[str], tables: list[tuple[str, dict]]) -> Path:
    path = MODELS_TOML_PATH
    if path.exists():
        shutil.copy2(path, path.with_name(path.name + ".bak"))
    text = write_toml_stdlib(path, provider_ids, tables)
    validate_toml_text(text)
    try:
        atomic_write(path, text)
    except OSError as exc:
        fail(f"failed to write {path}: {exc}")
    return path


def run_sync() -> tuple[Path | None, dict]:
    """Reconcile providers.json with the live API and (re)write models.toml."""
    providers_doc = load_providers()
    if not providers_doc["providers"]:
        print("No providers in providers.json. Use --add-provider to add one.")
        return None, {}
    api = fetch_models_dev()

    stats = {
        "providers_synced": 0,
        "models_added": 0,
        "models_removed": 0,
        "models_missing": 0,
        "providers_missing": 0,
        "tables_written": 0,
    }

    all_provider_ids = [
        p["id"] for p in providers_doc["providers"] if isinstance(p, dict) and p.get("id")
    ]
    # The script owns [model.*] tables for any models.dev provider: current
    # providers plus anything the live API exposes (so deleting a provider
    # also strips its leftover tables on the next write).
    managed_ids = set(all_provider_ids) | set(api.keys())
    tables: list[tuple[str, dict]] = []
    changed = False

    for provider in providers_doc["providers"]:
        if not isinstance(provider, dict) or not provider.get("id"):
            continue
        if not provider.get("enabled", True):
            continue
        pid = provider["id"]
        pinfo = api.get(pid)
        if not isinstance(pinfo, dict):
            print(f"  warning: provider {pid!r} not found in models.dev; skipping")
            stats["providers_missing"] += 1
            continue
        api_models = pinfo.get("models") if isinstance(pinfo.get("models"), dict) else {}

        models_map = provider.get("models")
        if not isinstance(models_map, dict):
            models_map = {}
            provider["models"] = models_map
            changed = True

        for mid in api_models:
            if mid not in models_map:
                models_map[mid] = {"enabled": False}
                stats["models_added"] += 1
                changed = True
        for mid in list(models_map):
            if mid not in api_models:
                del models_map[mid]
                stats["models_removed"] += 1
                changed = True

        base_url = pinfo.get("api") or ""
        env_key = first_env_key(pinfo)
        pname = pinfo.get("name") or pid
        if not base_url:
            print(
                f"  warning: provider {pid!r} has no base URL (api) in models.dev; "
                f"tables will have an empty base_url"
            )

        for mid, m in models_map.items():
            if not isinstance(m, dict):
                m = models_map[mid] = {"enabled": False}
                changed = True
            if not m.get("enabled", True):
                continue
            minfo = api_models.get(mid)
            if not isinstance(minfo, dict):
                print(f"  warning: model {mid!r} not found in models.dev; skipping")
                stats["models_missing"] += 1
                continue
            fields = build_fields(mid, minfo, base_url, env_key, pname)
            table_key = table_model_id(pid, mid)
            tables.append((table_key, fields))
            stats["tables_written"] += 1
        stats["providers_synced"] += 1

    if changed:
        dump_json(PROVIDERS_PATH, providers_doc)

    path = write_models_toml(managed_ids, tables)
    return path, stats


def cmd_add_provider() -> int:
    providers_doc = load_providers()
    api = fetch_models_dev()
    existing = {p.get("id") for p in providers_doc["providers"] if isinstance(p, dict)}

    provider_id: str | None = None
    while True:
        raw = prompt_line("Provider id (or type 'search' to search providers)")
        if raw.strip().lower() == "search":
            provider_id = search_providers(api)
            if provider_id is None:
                continue
        else:
            provider_id = raw.strip()
        if not provider_id:
            continue
        if provider_id in existing:
            print(f"Provider {provider_id!r} already exists.")
            continue
        break

    pinfo = api.get(provider_id)
    if not isinstance(pinfo, dict):
        fail(f"provider {provider_id!r} not found in models.dev")
    api_models = pinfo.get("models") if isinstance(pinfo.get("models"), dict) else {}
    if not api_models:
        fail(f"provider {provider_id!r} has no models in models.dev")

    models_map = {mid: {"enabled": False} for mid in api_models}
    entry = {
        "id": provider_id,
        "name": pinfo.get("name") or provider_id,
        "enabled": True,
        "models": models_map,
    }
    providers_doc["providers"].append(entry)
    dump_json(PROVIDERS_PATH, providers_doc)
    print(f"Added provider {provider_id!r} with {len(models_map)} models (all enabled).")

    answer = prompt_line("Sync now?", "Y")
    parsed = parse_bool(answer) if answer else True
    if parsed is None:
        parsed = True
    if parsed:
        path, stats = run_sync()
        if path is not None:
            print_summary(stats, path)
            print_relaunch()
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
        dump_json(PROVIDERS_PATH, providers_doc)
    enabled = sum(1 for mid in ids if models[mid].get("enabled", True))
    print(f"Updated models for {selected['id']!r}: {enabled} enabled of {len(ids)}.")
    return changed


def _confirm_delete(pid: str) -> bool:
    while True:
        confirm = prompt_line(f"Delete provider {pid!r}? [no]", "no")
        parsed = parse_bool(confirm) if confirm else False
        if parsed is None:
            print("Enter yes or no.")
            continue
        return parsed


def _numbered_config_flow(providers_doc: dict, providers: list) -> bool:
    """Numbered (non-TTY) fallback for the entire --config flow."""
    changed = False
    while True:
        labels = [_provider_label(p) for p in providers]
        pi = _numbered_select(labels, "Select a provider  (q cancels)")
        if pi is None:
            break
        selected = providers[pi]
        while True:
            enabled = bool(selected.get("enabled", True))
            actions = [
                "Configure models",
                "Disable provider" if enabled else "Enable provider",
                "Delete provider",
                "Back",
            ]
            ai = _numbered_select(actions, f"Provider: {selected['id']}  (q cancels)")
            if ai is None or actions[ai] == "Back":
                break
            if ai == 0:
                if _config_models(selected, providers_doc):
                    changed = True
            elif ai == 1:
                selected["enabled"] = not enabled
                dump_json(PROVIDERS_PATH, providers_doc)
                verb = "Disabled" if enabled else "Enabled"
                print(f"{verb} provider {selected['id']!r}.")
                changed = True
            elif ai == 2:
                if _confirm_delete(selected["id"]):
                    providers_doc["providers"] = [
                        p
                        for p in providers_doc["providers"]
                        if p.get("id") != selected["id"]
                    ]
                    dump_json(PROVIDERS_PATH, providers_doc)
                    print(f"Deleted provider {selected['id']!r}.")
                    changed = True
                    return changed
    return changed


def cmd_config() -> int:
    providers_doc = load_providers()
    providers = [p for p in providers_doc["providers"] if isinstance(p, dict) and p.get("id")]
    if not providers:
        print("No providers in providers.json. Use --add-provider to add one.")
        return 0

    changed = False
    if sys.stdin.isatty() and sys.stdout.isatty():
        r = _curses_config_flow(providers_doc, providers)
        if r is _CURSES_FAILED:
            changed = _numbered_config_flow(providers_doc, providers)
        else:
            changed = bool(r)
    else:
        changed = _numbered_config_flow(providers_doc, providers)

    if changed:
        path, stats = run_sync()
        if path is not None:
            print_summary(stats, path)
            print_relaunch()
    return 0


def print_summary(stats: dict, path: Path) -> None:
    print()
    print("Summary")
    print(f"  providers synced: {stats.get('providers_synced', 0)}")
    print(f"  models added: {stats.get('models_added', 0)}")
    print(f"  models removed: {stats.get('models_removed', 0)}")
    print(f"  models missing (skipped): {stats.get('models_missing', 0)}")
    print(f"  providers missing (skipped): {stats.get('providers_missing', 0)}")
    print(f"  tables written: {stats.get('tables_written', 0)}")
    print(f"Wrote {path}")


def print_relaunch() -> None:
    print(
        "Quit and relaunch Grok Build. A new session in the same process "
        "will not reload models.toml."
    )


def cmd_sync() -> int:
    path, stats = run_sync()
    if path is None:
        return 0
    print_summary(stats, path)
    print_relaunch()
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Sync Grok Build [model.*] tables from models.dev."
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--add-provider", action="store_true", help="Interactively add a provider")
    group.add_argument(
        "--config",
        action="store_true",
        help="Configure a provider (enable/disable/delete) or its models (toggle enabled)",    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.add_provider:
            return cmd_add_provider()
        if args.config:
            return cmd_config()
        return cmd_sync()
    except SyncError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("Interrupted.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
