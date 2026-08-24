#!/usr/bin/env python3
"""Sync Grok Build [model.*] tables from models.dev.

Providers and their models are tracked in `providers.json`. Model metadata
(base URL, env key, context window, reasoning) is taken live from
https://models.dev/api.json so no separate model cache is needed.
"""

from __future__ import annotations

import argparse
import bisect
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


# Canonical layout for providers.json. Every read and write goes through
# these shapes, so entries come out identical no matter which code path
# (import, add-provider, sync) produced them: fields in canonical order,
# providers alphabetically by display name, models alphabetically by
# display name.
TOP_LEVEL_KEY_ORDER = ("providers", "removed_providers")
PROVIDER_KEY_ORDER = ("id", "name", "env_key", "base_url", "enabled", "models")
MODEL_KEY_ORDER = ("name", "enabled")

# Code-panel padding. Horizontal padding is char-exact; vertical granularity
# is whole terminal rows (a row is visually taller than a column, so keep
# CODE_PANEL_PAD_Y low if you want it to feel even with the sides).
CODE_PANEL_PAD_X = 1
CODE_PANEL_PAD_Y = 1


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
    """Single write path for providers.json: emits every provider/model entry
    in canonical key order, providers alphabetically by id and models
    alphabetically by display name, regardless of which code path produced
    them."""
    ordered = order_keys(doc, TOP_LEVEL_KEY_ORDER)
    providers = [
        order_provider_entry(pr) if isinstance(pr, dict) else pr
        for pr in ordered.get("providers", [])
    ]
    providers.sort(key=_provider_sort_key)
    ordered["providers"] = providers
    dump_json(path, ordered)


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
    data["providers"] = [
        order_provider_entry(p) if isinstance(p, dict) else p
        for p in data["providers"]
    ]
    data["providers"].sort(key=_provider_sort_key)
    if not isinstance(data["providers"], list):
        fail("providers.json: 'providers' must be a list")
    return data


def http_get_json(url: str) -> object:
    headers = {"User-Agent": "grok-models.py", "Accept": "application/json"}
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


def search_providers(api: dict, term: str) -> str | None:
    """Search the models.dev provider list with term; return a chosen id."""
    term_l = term.lower()
    matches: list[tuple[str, str]] = []
    for pid, pinfo in api.items():
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

    # Code-block palette (macOS Terminal "Homebrew"-style): standard ANSI
    # colors only, so the block renders identically on every terminal —
    # pure black background, green font, yellow operators/quotes, cyan
    # comments, white strings, red unset vars.
    code_bg = curses.COLOR_BLACK
    code_text = curses.COLOR_GREEN
    code_comment = curses.COLOR_CYAN
    code_var = curses.COLOR_GREEN
    # ANSI COLOR_YELLOW is olive/brown on most terminals; gold reads as yellow.
    _code_symbol = rgb(255, 204, 0)
    _code_error = red  # original Tokyo Night red for unset env vars
    code_string = curses.COLOR_WHITE

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
    while True:
        stdscr.erase()
        height, width = stdscr.getmaxyx()
        safe_w = max(1, width - 1)
        _curses_theme_bkgd(stdscr)
        
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
            line = _clip_cols(line, max(1, width - 2))

            is_sel = (idx == current)
            try:
                # Row background first (theme bg, or selection bg for the cursor),
                # then the label. A chevron sits right-aligned on expandable rows.
                row_bg = curses.color_pair(P.SELECTED if is_sel else P.TEXT)
                stdscr.addstr(2 + row, 0, "\u00a0" * (width - 1), row_bg)
                label_attr = row_bg | curses.A_BOLD if is_sel and not multi else row_bg
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
                if token:
                    pos = line.index(token)
                    head = line[:pos]
                    tail = line[pos + len(token):]
                    tok_attr = curses.color_pair(tcolor) | (
                        curses.A_BOLD if is_sel else 0
                    )
                    _addstr_cols(stdscr, 2 + row, 0, _clip_cols(head, width - 2), label_attr)
                    hx = _str_cols(head)
                    _addstr_cols(
                        stdscr,
                        2 + row,
                        hx,
                        _clip_cols(token, max(0, (width - 2) - hx)),
                        tok_attr,
                    )
                    if tail:
                        tx = hx + _str_cols(token)
                        _addstr_cols(
                            stdscr,
                            2 + row,
                            tx,
                            _clip_cols(tail, max(0, (width - 2) - tx)),
                            label_attr,
                        )
                else:
                    _addstr_cols(
                        stdscr, 2 + row, 0,
                        _pad_cols(line, width - 1),
                        label_attr,
                    )
                if not multi:
                    chev_x = max(width - 4, _str_cols(line) + 2)
                    stdscr.addstr(
                        2 + row,
                        chev_x,
                        "›",
                        curses.color_pair(P.CHEVRON),
                    )
            except curses.error:
                pass
        
        # Separator line
        sep_y = 2 + min(n, height - 4)
        try:
            stdscr.addstr(sep_y, 0, "─" * (width - 1), curses.color_pair(P.CHEVRON))
        except curses.error:
            pass

        # Models preview: fill the empty space below the list (the --config
        # main menu) with the enabled-models listing, styled like --models.
        if preview:
            avail_top = sep_y + 1
            # Reserve two rows above the legend for the transient status line.
            avail_bottom = height - (5 if status else 3)
            max_lines = avail_bottom - avail_top + 1
            if max_lines > 0:
                truncated = len(preview) > max_lines
                draw_lines = preview[:max_lines]
                if truncated:
                    draw_lines = draw_lines[:-1] + [[("… (run --models for all)", P.MUTED)]]
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
                        _draw_seg_line(stdscr, y, 2, segs, width - 3)

        # Transient status line (e.g. post-add confirmation), kept a few rows
        # above the legend so long messages never clobber the menu chrome.
        if status:
            try:
                stdscr.addstr(
                    height - 4,
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
        legend = [("↑/↓", "nav"), ("Enter/→", "select")]
        if multi:
            legend.append(("Space", "toggle"))
        if back_on_left:
            legend.append(("←", "back"))
        else:
            # Menus without a back binding (main menu) quit via q instead;
            # render it inline like the other bindings.
            legend.append(("Q", "quit"))
        _curses_draw_legend(stdscr, legend)
        
        stdscr.refresh()
        _emit_sgr_bg()
        ch = stdscr.getch()
        if ch == curses.KEY_RESIZE:
            _curses_theme_bkgd(stdscr)
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
            row_y = 2 + (current - top)
            prefix = options[current].split("[", 1)[0]
            _curses_draw_legend(stdscr, [("Enter", "save"), ("ESC", "cancel")])
            while True:
                open_text = f"{prefix}[{''.join(buf)}"[: max(1, edit_w - 3)]
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
            return sorted(state) if multi else current
        elif back_on_left and ch == curses.KEY_LEFT:
            return None
        elif ch == 27 and back_on_left:
            # ESC goes back in submenus
            return None
        elif ch == ord("q") and not back_on_left:
            # q quits the tool; only bound at the main menu
            return None


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


def _curses_filter_list_win(
    entries: list,
    stdscr,
    *,
    title: str,
    legend: list,
    compute_view,
    render,
    on_enter=None,
) -> None:
    """Generic type-to-filter list widget drawn into an existing stdscr.
    compute_view(entries, query) -> (ordered_entries, separators); separators
    is [(row_index_after_which, color_pair)] drawn when that boundary row is
    visible. render(entry, is_selected) -> (text, color_pair).
    on_enter(entry) -> bool: True keeps the window open, False closes it.
    ESC or Left-at-top always closes."""
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
    while True:
        filtered, separators = compute_view(entries, query)
        if not filtered:
            current = 0
        elif current >= len(filtered):
            current = len(filtered) - 1
        stdscr.erase()
        height, width = stdscr.getmaxyx()
        _curses_theme_bkgd(stdscr)

        # Header with filter
        _curses_draw_header(
            stdscr, f"  {title}  ({len(filtered)})  |  Filter: {query}"
        )

        list_top = 2
        list_h = max(1, height - list_top - 2)
        if current < top:
            top = current
        elif current >= top + list_h:
            top = current - list_h + 1

        if not filtered:
            try:
                stdscr.addstr(2, 0, "  (no matches)", curses.color_pair(P.MUTED))
            except curses.error:
                pass

        for row in range(list_h):
            idx = top + row
            if idx >= len(filtered):
                break
            entry = filtered[idx]
            line, row_pair = render(entry, idx == current)
            try:
                stdscr.addstr(2 + row, 0, "\u00a0" * (width - 1), curses.color_pair(row_pair))
                _addstr_cols(
                    stdscr, 2 + row, 0,
                    _pad_cols(_clip_cols(line, width - 2), width - 1),
                    curses.color_pair(row_pair),
                )
            except curses.error:
                pass

        for sep_idx, sep_pair in separators:
            if 0 < sep_idx < len(filtered) and top <= sep_idx - 1 < top + list_h:
                y = 2 + sep_idx - top
                try:
                    stdscr.addstr(y, 0, "─" * (width - 1), curses.color_pair(sep_pair))
                except curses.error:
                    pass

        _curses_draw_legend(stdscr, legend)

        stdscr.refresh()
        _emit_sgr_bg()
        ch = stdscr.getch()
        if ch == curses.KEY_RESIZE:
            _curses_theme_bkgd(stdscr)
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
                top = min((current // list_h + 1) * list_h, max(0, len(filtered) - 1))
                current = top
        elif ch == curses.KEY_LEFT:
            if current == 0:
                # At the very top of the first page: left goes back
                return
            if current < list_h:
                # Already on the first page: left just goes to its top
                top = 0
                current = 0
            else:
                # Page up: full previous page, first row selected
                top = ((current // list_h) - 1) * list_h
                current = top
        elif ch in (curses.KEY_BACKSPACE, 127, 8):
            query = query[:-1]
            current = 0
            top = 0
        elif ch in (curses.KEY_ENTER, 10, 13):
            if filtered and on_enter is not None:
                if not on_enter(filtered[current]):
                    return
        elif 32 <= ch <= 126:
            query += chr(ch)
            current = 0
            top = 0


def _curses_model_search_win(
    ids: list[str], models: dict, stdscr, provider_title: str,
) -> bool:
    """Model picker built on _curses_filter_list_win: type to filter model ids
    live, arrow to move, Enter toggles the selected model's enabled state,
    q/ESC finishes. Left/Right arrows page.
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

    def render(mid, is_sel):
        m = models[mid]
        enabled = bool(m.get("enabled", True)) if isinstance(m, dict) else False
        is_free = "free" in mid.lower()
        mark = "●" if enabled else "○"
        free_tag = "  [free]" if is_free and not enabled else ""
        pair = P.SELECTED if is_sel else (
            P.ENABLED if enabled else (P.FREE if is_free else P.DISABLED)
        )
        return f"  {mark}  {mid}{free_tag}", pair

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
        title=f"{provider_title} | Configure models",
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
    height, width = stdscr.getmaxyx()
    safe_w = max(1, width - 1)
    try:
        _curses_draw_header(stdscr, "  Confirm")
        stdscr.addstr(2, 2, prompt[:width - 4], curses.color_pair(P.TEXT))
        legend = [("Y", "yes"), ("N", "no"), ("ESC", "cancel")]
        _curses_draw_legend(stdscr, legend)
    except curses.error:
        pass
    stdscr.refresh()
    _emit_sgr_bg()
    while True:
        ch = stdscr.getch()
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
    """Modal: type-to-filter the models.dev catalog and add a provider.
    Returns True if a provider was added. Fetch/add errors surface inline so
    the surrounding TUI session survives."""
    try:
        api = fetch_models_dev()
    except SyncError as exc:
        _curses_inline_error_win(stdscr, f"Fetch failed: {exc}")
        return False

    existing = {
        p.get("id")
        for p in providers_doc["providers"]
        if isinstance(p, dict)
    }
    catalog = sorted(
        (pid, pinfo.get("name") or "")
        for pid, pinfo in api.items()
        if isinstance(pinfo, dict) and pid not in existing
    )

    result = {"added": None}

    def compute_view(entries, query):
        return (
            [e for e in entries if _provider_matches(e[0], e[1], query.lower())],
            [],
        )

    def render(entry, is_sel):
        pid, name = entry
        label = f"  {pid} ({name})" if name else f"  {pid}"
        return label, (P.SELECTED if is_sel else P.TEXT)

    def add(entry):
        pid = entry[0]
        before = len(providers_doc["providers"])
        try:
            add_provider_entry(providers_doc, api, pid, quiet=True)
        except SyncError as exc:
            _curses_inline_error_win(stdscr, f"Add failed: {exc}")
            return True  # stay open
        if len(providers_doc["providers"]) > before:
            # Mirror the new entry into the live list so the parent menu
            # refreshes, inserting at its sorted position by display name.
            new_entry = providers_doc["providers"][-1]
            keys = [_provider_sort_key(p) for p in providers]
            providers.insert(bisect.bisect(keys, _provider_sort_key(new_entry)), new_entry)
            result["added"] = (
                f"Added provider '{pid}' with "
                f"{len(new_entry.get('models', {}))} models (all disabled)."
            )
        return False  # close back to the provider menu

    _curses_filter_list_win(
        catalog, stdscr,
        title="Add provider",
        legend=[("↑/↓/←/→", "nav"), ("ESC", "cancel"), ("Enter", "add"), ("type", "filter")],
        compute_view=compute_view,
        render=render,
        on_enter=add,
    )
    return result["added"]


def _curses_add_model_win(providers_doc: dict, providers: list, stdscr) -> str | None:
    """Modal: type-to-filter every models.dev model across all providers and
    enable the chosen one. Selecting a model of a provider that has not been
    added yet adds that provider first (all its other models disabled), then
    enables just the chosen model. Returns a confirmation status line for the
    parent menu, or None. Fetch/add errors surface inline so the surrounding
    TUI session survives."""
    try:
        api = fetch_models_dev()
    except SyncError as exc:
        _curses_inline_error_win(stdscr, f"Fetch failed: {exc}")
        return None

    # Index of combos already enabled in providers.json, so the catalog can
    # skip them in one lookup (mirrors Add Provider excluding existing).
    enabled_combos = set()
    for p in providers_doc["providers"]:
        if not isinstance(p, dict) or not p.get("id"):
            continue
        mm = p.get("models") if isinstance(p.get("models"), dict) else {}
        for mid0, m0 in mm.items():
            if isinstance(m0, dict) and bool(m0.get("enabled", True)):
                enabled_combos.add((p.get("id"), mid0))

    # Flatten the catalog across every provider; skip already-enabled combos.
    catalog = []
    for pid, pinfo in api.items():
        if not isinstance(pinfo, dict):
            continue
        pname = pinfo.get("name") or pid
        api_models = pinfo.get("models") if isinstance(pinfo.get("models"), dict) else {}
        for mid, minfo in api_models.items():
            if (pid, mid) in enabled_combos:
                continue
            mname = minfo.get("name") if isinstance(minfo, dict) else None
            catalog.append((pid, mid, mname or mid, str(pname)))
    catalog.sort(key=lambda e: (e[2].lower(), e[0], e[1]))

    result = {"status": None}

    def compute_view(entries, query):
        term_l = query.lower()

        def matches(entry):
            return (
                not term_l
                or term_l in entry[2].lower()  # model display name
                or term_l in entry[1].lower()  # model id
            )

        return [e for e in entries if matches(e)], []

    def render(entry, is_sel):
        pid, mid, mname, pname = entry
        return f"  {mname} ({pname}) - {pid}/{mid}", (
            P.SELECTED if is_sel else P.TEXT
        )

    def enable(entry):
        pid, mid, mname, pname = entry
        existing = {
            p.get("id") for p in providers_doc["providers"] if isinstance(p, dict)
        }
        added = False
        if pid not in existing:
            before = len(providers_doc["providers"])
            try:
                add_provider_entry(providers_doc, api, pid, quiet=True)
            except SyncError as exc:
                _curses_inline_error_win(stdscr, f"Add failed: {exc}")
                return True  # stay open
            if len(providers_doc["providers"]) > before:
                # Mirror the new entry into the live list so the parent menu
                # refreshes, inserting at its sorted position by display name.
                new_entry = providers_doc["providers"][-1]
                keys = [_provider_sort_key(p) for p in providers]
                providers.insert(
                    bisect.bisect(keys, _provider_sort_key(new_entry)), new_entry
                )
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
        dump_providers(PROVIDERS_PATH, providers_doc)
        prefix = f"Added provider '{pid}'. " if added else ""
        result["status"] = f"{prefix}Enabled {mname} ({pname}) - {pid}/{mid}."
        return False  # close back to the main menu

    _curses_filter_list_win(
        catalog, stdscr,
        title="Add model",
        legend=[
            ("↑/↓/←/→", "nav"),
            ("ESC", "cancel"),
            ("Enter", "enable"),
            ("type", "filter"),
        ],
        compute_view=compute_view,
        render=render,
        on_enter=enable,
    )
    return result["status"]


def _curses_config_flow(providers_doc: dict, providers: list) -> bool | object:
    """Run the whole --config flow inside ONE curses session so there is no
    terminal-mode flash between menus. Returns True if providers.json
    changed, False if not, or _CURSES_FAILED on any curses error."""
    import curses

    def main(stdscr) -> bool:
        changed = False
        status_msg = None
        while True:
            # providers is already name-sorted by load_providers(); keep it.
            ordered = providers
            labels = [_provider_label(p) for p in ordered]
            labels.append("➕ Add provider…")
            labels.append("➕ Add model…")
            pi = _curses_select_win(
                stdscr, labels, "Select Provider",
                status=status_msg,
                preview=_build_config_models_preview(providers_doc),
            )
            if pi is None:
                return changed
            if pi == len(ordered):
                added_msg = _curses_add_provider_win(providers_doc, providers, stdscr)
                if added_msg:
                    status_msg = added_msg
                    changed = True
                continue
            if pi == len(ordered) + 1:
                enabled_msg = _curses_add_model_win(providers_doc, providers, stdscr)
                if enabled_msg:
                    status_msg = enabled_msg
                    changed = True
                continue
            status_msg = None
            selected = ordered[pi]
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
                    "Configure models",
                    f"Provider [{'enabled' if enabled else 'disabled'}]",
                    f"Base Url [{_bu_get()}]",
                    "Delete provider",
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
                    ):
                        dump_providers(PROVIDERS_PATH, providers_doc)
                        changed = True
                elif ai == 1:
                    selected["enabled"] = not enabled
                    dump_providers(PROVIDERS_PATH, providers_doc)
                    changed = True
                elif ai == 3:
                    if _curses_confirm_win(stdscr, f"Delete provider {selected['id']!r}?"):
                        providers_doc["providers"] = [
                            p
                            for p in providers_doc["providers"]
                            if p.get("id") != selected["id"]
                        ]
                        _record_removed_provider(providers_doc, selected["id"])
                        providers[:] = [
                            p for p in providers if p.get("id") != selected["id"]
                        ]
                        dump_providers(PROVIDERS_PATH, providers_doc)
                        changed = True
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
    return f"{p['id']} ({p.get('name') or p['id']}) [{state}]"


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
        order, _, _ = _sort_model_indices(ids, models_map or {})
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
        for idx in order:
            mid = ids[idx]
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
    # Providers sorted alphabetically by name; models within each provider
    # sorted alphabetically by display name.
    for provider in sorted(
        providers, key=lambda p: (p.get("name") or p["id"]).lower()
    ):
        pid = provider["id"]
        penabled = bool(provider.get("enabled", True))
        mm = provider.get("models")
        if not isinstance(mm, dict):
            continue
        pname = provider.get("name") or pid
        rows = []
        for mid, m in mm.items():
            if not isinstance(m, dict) or not m.get("enabled", True):
                continue
            if not penabled:
                continue
            mname = m.get("name") or mid
            rows.append((mname.lower(), mname, pid, mid))
            total_enabled += 1
        rows.sort(key=lambda r: (r[0], r[2], r[3]))
        for _, mname, pid, mid in rows:
            print(f"● {mname} ({pname}) - {pid}/{mid}")

    if not total_enabled:
        print("No enabled models. Enable with --enable or grok-models")
        return 0

    print()
    for provider in providers:
        env = first_env_key(provider)
        if env:
            penabled = bool(provider.get("enabled", True))
            marker = "●" if penabled else "○"
            pname = provider.get("name") or provider["id"]
            print(f"{marker} Required env var: {env} = {_env_value(env)}  ({pname})")
    print(f"Summary: {total_enabled} models enabled")
    return 0
    print(f"Summary: {total_enabled} models enabled")
    return 0


def _build_config_models_preview(providers_doc: dict) -> list:
    """Build the --models-style enabled-models listing as colored segment
    lines, for rendering in the empty space under the --config main menu."""
    providers = [
        p for p in providers_doc.get("providers", [])
        if isinstance(p, dict) and p.get("id")
    ]
    lines: list = []
    # First element is a heading marker: ("heading", text) -> drawn as a
    # full-width blue bar, like the screen title.
    lines.append(("heading", "Enabled Models"))
    lines.append([("", P.TEXT)])  # gap under the models header
    total_enabled = 0
    # Grouped by provider: providers sorted alphabetically by name, models
    # within each provider sorted alphabetically by display name.
    prov_sorted = sorted(
        [p for p in providers if isinstance(p, dict) and p.get("id")],
        key=lambda p: (p.get("name") or p["id"]).lower(),
    )
    for provider in prov_sorted:
        pid = provider["id"]
        penabled = bool(provider.get("enabled", True))
        mm = provider.get("models")
        if not isinstance(mm, dict):
            continue
        pname = provider.get("name") or pid
        rows = []
        for mid, m in mm.items():
            if not isinstance(m, dict) or not m.get("enabled", True):
                continue
            if not penabled:
                continue
            mname = m.get("name") or mid
            rows.append((mname.lower(), mname, pname, pid, mid))
            total_enabled += 1
        rows.sort(key=lambda r: (r[0], r[2], r[3]))
        for _, mname, pname, pid, mid in rows:
            lines.append([
                ("● ", P.ENABLED),
                (mname, P.VALUE),
                (f" ({pname}) - {pid}/{mid}", P.TEXT),
            ])
    if not total_enabled:
        lines.append([("No enabled models. Enable with --enable or grok-models", P.MUTED)])
        return lines
    lines.append([("", P.TEXT)])
    # Env-var requirements rendered as a borderless black code panel with
    # padding: green text, gray provider-name annotations, red for unset keys.
    env_rows = []
    for provider in prov_sorted:
        env = first_env_key(provider)
        if not env:
            continue
        val = _env_value(env)
        pname = provider.get("name") or provider["id"]
        env_rows.append((env, val, pname, val == '""'))
    if env_rows:
        w_env = max(len(e) for e, _, _, _ in env_rows)
        w_val = max(len(v) for _, v, _, _ in env_rows)
        rows_segs = [_code_line_segments("# required env_key values")]
        for env, val, pname, missing in env_rows:
            body = env.ljust(w_env) + " = " + val.ljust(w_val)
            highlight = (0, w_env, P.CODE_ERROR) if missing else None
            segs = _code_line_segments(body, highlight=highlight)
            # Provider name renders as a shell comment, e.g. "  # OpenRouter".
            segs.append(("  # " + pname, P.CODE_COMMENT))
            rows_segs.append(segs)
        panel_w = (
            max(sum(len(t) for t, _ in segs) for segs in rows_segs)
            + 2 * CODE_PANEL_PAD_X
        )
        for segs in rows_segs:
            seg_len = sum(len(t) for t, _ in segs)
            lines.append(
                [(" " * CODE_PANEL_PAD_X, P.CODE_TEXT)]
                + segs
                + [(" "
                    * max(0, panel_w - CODE_PANEL_PAD_X - seg_len),
                    P.CODE_TEXT)]
            )
    lines.append([(f"Summary: {total_enabled} models enabled", P.MUTED)])
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


def _record_removed_provider(providers_doc: dict, pid: str) -> None:
    """Record a deleted provider id so sync strips its leftover tables."""
    removed = providers_doc.setdefault("removed_providers", [])
    if pid not in removed:
        removed.append(pid)


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


def write_config_toml(provider_ids: list[str], tables: list[tuple[str, dict]]) -> Path:
    path = CONFIG_TOML_PATH
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
    """Reconcile providers.json with the live API and (re)write ~/.grok/config.toml."""
    providers_doc = load_providers()
    if not providers_doc["providers"]:
        if providers_doc.get("removed_providers"):
            # Still strip tables left behind by deleted providers.
            api = fetch_models_dev()
            write_config_toml(set(providers_doc["removed_providers"]), [])
            providers_doc["removed_providers"] = []
            dump_providers(PROVIDERS_PATH, providers_doc)
        print("No providers configured yet. Add with --add-provider")
        return None, {}
    api = fetch_models_dev()

    stats = {
        "providers_synced": 0,
        "models_added": 0,
        "models_removed": 0,
        "models_renamed": 0,
        "models_missing": 0,
        "providers_missing": 0,
        "tables_written": 0,
    }

    all_provider_ids = [
        p["id"] for p in providers_doc["providers"] if isinstance(p, dict) and p.get("id")
    ]
    # This tool owns [model.*] tables only for providers it has configured.
    # Deleted providers are remembered in "removed_providers" so the next
    # sync strips their leftover tables.
    managed_ids = set(all_provider_ids) | set(providers_doc.get("removed_providers", []))
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

        new_env_key = api_env_key(pinfo)
        if new_env_key and provider.get("env_key") != new_env_key:
            provider["env_key"] = new_env_key
            changed = True

        models_map = provider.get("models")
        if not isinstance(models_map, dict):
            models_map = {}
            provider["models"] = models_map
            changed = True

        for mid in api_models:
            if mid not in models_map:
                entry = {}
                api_name = (
                    api_models[mid].get("name")
                    if isinstance(api_models[mid], dict) else None
                )
                if api_name:
                    entry["name"] = api_name
                entry["enabled"] = False
                models_map[mid] = entry
                stats["models_added"] += 1
                changed = True
            else:
                m = models_map[mid]
                if isinstance(m, dict):
                    api_name = api_models[mid].get("name") if isinstance(api_models[mid], dict) else None
                    if api_name and m.get("name") != api_name:
                        m["name"] = api_name
                        stats["models_renamed"] += 1
                        changed = True
        for mid in list(models_map):
            if mid not in api_models:
                del models_map[mid]
                stats["models_removed"] += 1
                changed = True

        # A stored non-empty base_url wins over the catalog; missing/empty
        # backfills from the catalog and counts as a change (persisted by the
        # trailing dump, and it rewrites config.toml with the effective URL).
        stored = provider.get("base_url")
        if not isinstance(stored, str):
            stored = ""
        catalog_api = pinfo.get("api") or ""
        if not stored and catalog_api:
            provider["base_url"] = catalog_api
            stored = catalog_api
            changed = True
        base_url = stored
        env_key = api_env_key(pinfo)
        pname = pinfo.get("name") or pid
        if not base_url:
            print(
                f"  warning: provider {pid!r} has no base URL (api) in models.dev; "
                f"tables will have an empty base_url"
            )

        for mid, m in models_map.items():
            if not isinstance(m, dict):
                api_name = (
                    api_models.get(mid, {}).get("name")
                    if isinstance(api_models.get(mid), dict) else None
                )
                entry = {"name": api_name} if api_name else {}
                entry["enabled"] = False
                m = models_map[mid] = entry
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

    # Strip tables for providers deleted since the last sync.
    removed = providers_doc.get("removed_providers", [])
    if removed:
        removed_keys = {
            table_model_id(pid, mid)
            for pid in removed
            for mid in api.get(pid, {}).get("models", {})
        }
        tables = [t for t in tables if t[0] not in removed_keys]
        providers_doc["removed_providers"] = []
        changed = True

    if changed:
        dump_providers(PROVIDERS_PATH, providers_doc)

    path = write_config_toml(managed_ids, tables)
    return path, stats


def print_sync_report(stats: dict, path: Path, providers_doc: dict) -> None:
    """Sync summary followed by required env vars for enabled providers."""
    print_summary(stats, path)
    print_env_requirements(providers_doc)


def add_provider_entry(
    providers_doc: dict, api: dict, provider_id: str, quiet: bool = False
) -> None:
    """Add provider_id to providers_doc with all models disabled and persist.
    quiet suppresses stdout reports — required when called inside the curses
    TUI, where any raw print corrupts the screen."""
    existing = {p.get("id") for p in providers_doc["providers"] if isinstance(p, dict)}
    if provider_id in existing:
        if not quiet:
            print(f"Provider {provider_id!r} already exists.")
        return
    pinfo = api.get(provider_id)
    if not isinstance(pinfo, dict):
        fail(f"provider {provider_id!r} not found in models.dev")
    api_models = pinfo.get("models") if isinstance(pinfo.get("models"), dict) else {}
    if not api_models:
        fail(f"provider {provider_id!r} has no models in models.dev")

    models_map = {}
    for mid, minfo in api_models.items():
        entry = {}
        if isinstance(minfo, dict) and minfo.get("name"):
            entry["name"] = minfo["name"]
        entry["enabled"] = False
        models_map[mid] = entry
    entry = {
        "id": provider_id,
        "name": pinfo.get("name") or provider_id,
    }
    env = api_env_key(pinfo)
    if env:
        entry["env_key"] = env
    api_base = pinfo.get("api")
    if isinstance(api_base, str) and api_base:
        entry["base_url"] = api_base
    entry["enabled"] = True
    entry["models"] = models_map
    providers_doc["providers"].append(entry)
    dump_providers(PROVIDERS_PATH, providers_doc)
    if not quiet:
        print(f"Added provider {provider_id!r} with {len(models_map)} models (all disabled).")


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

    # Providers that already exist are skipped by add-provider ("already
    # exists"); per the revised flow they get enabled so run_sync reconciles
    # them against the API (adds missing models, drops dead ones) before the
    # per-model enables run.
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
        # providers is already name-sorted by load_providers(); keep it.
        ordered = providers
        labels = [_provider_label(p) for p in ordered]
        pi = _numbered_select(labels, "Select a provider  (q quits)")
        if pi is None:
            break
        selected = ordered[pi]
        while True:
            enabled = bool(selected.get("enabled", True))
            actions = [
                "Configure models",
                f"{'Disable' if enabled else 'Enable'} provider",
                "Delete provider",
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
                if _confirm_delete(selected["id"]):
                    providers_doc["providers"] = [
                        p
                        for p in providers_doc["providers"]
                        if p.get("id") != selected["id"]
                    ]
                    _record_removed_provider(providers_doc, selected["id"])
                    providers[:] = [
                        p for p in providers if p.get("id") != selected["id"]
                    ]
                    dump_providers(PROVIDERS_PATH, providers_doc)
                    print(f"Deleted provider {selected['id']!r}.")
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
        if args.sync:
            return cmd_sync()
        # Default (no args): straight into the config TUI.
        return cmd_config()
    except SyncError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("Interrupted.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
