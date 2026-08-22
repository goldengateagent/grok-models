#!/usr/bin/env python3
"""Sync Grok Build [model.*] tables from models.dev.

Providers and their models are tracked in `providers.json`. Model metadata
(base URL, env key, context window, reasoning) is taken live from
https://models.dev/api.json so no separate model cache is needed.
"""

from __future__ import annotations

import argparse
import difflib
import os
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
CONFIG_TOML_PATH = Path(os.environ.get("GROK_HOME", Path.home() / ".grok")) / "config.toml"
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


def search_providers(api: dict, term: str) -> str | None:
    """Search the models.dev provider list with term; return a chosen id."""
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
    bg = rgb(*_TN["bg"])
    visual = rgb(*_TN["bg_visual"])

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
    }
    for pid, (f, b) in pairs.items():
        try:
            curses.init_pair(pid, f, b)
        except (curses.error, ValueError):
            break  # terminal reports fewer pairs than we need; stop cleanly


_TRUECOLOR_SLOT = [100]  # start above the 16 standard ANSI slots
_TRUECOLOR_MAP: dict[tuple, int] = {}


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
        stdscr.addstr(0, 0, " " * (width - 1), curses.color_pair(P.SELECTED))
        stdscr.addstr(0, 2, text[:width - 4], curses.color_pair(P.SELECTED) | curses.A_BOLD)
    except curses.error:
        pass


def _curses_draw_legend(stdscr, entries: list[tuple[str, str]]) -> None:
    """Draw the bottom legend: bold keys, gray descriptions, │ separators.

    entries is a list of (key, description) pairs, e.g. [("←/→", "move")].
    The full bottom row is painted with the theme background first so the
    line is themed edge to edge, not just where text sits.
    """
    height, width = stdscr.getmaxyx()
    stdscr.addstr(height - 1, 0, " " * (width - 1), curses.color_pair(P.TEXT))
    x = 2
    try:
        for i, (key, desc) in enumerate(entries):
            if i > 0:
                sep = "  │  "
                stdscr.addstr(height - 1, x, sep, curses.color_pair(P.MUTED))
                x += len(sep)
            run = f"{key} {desc}"
            if x + len(run) > width - 1:
                break
            stdscr.addstr(
                height - 1, x, key, curses.color_pair(P.LEGEND_KEY) | curses.A_BOLD
            )
            x += len(key)
            stdscr.addstr(height - 1, x, " ", curses.color_pair(P.LEGEND_DESC))
            x += 1
            stdscr.addstr(height - 1, x, desc, curses.color_pair(P.LEGEND_DESC))
            x += len(desc)
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
) -> int | list[int] | None:
    """curses selector drawn into an existing stdscr with color theme."""
    curses.set_escdelay(25)
    if not options:
        return None
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    _curses_init_colors()
    _curses_theme_bkgd(stdscr)
    state = set(preselected or [])
    current = 0
    n = len(options)
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
            line = line[:width - 2]

            is_sel = (idx == current)
            try:
                # Row background first (theme bg, or selection bg for the cursor),
                # then the label. A chevron sits right-aligned on expandable rows.
                row_bg = curses.color_pair(P.SELECTED if is_sel else P.TEXT)
                stdscr.addstr(2 + row, 0, " " * (width - 1), row_bg)
                label_attr = row_bg | curses.A_BOLD if is_sel and not multi else row_bg
                stdscr.addstr(2 + row, 0, line[: width - 2].ljust(width - 1), label_attr)
                if not multi:
                    chev_x = max(width - 4, len(line) + 2)
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

        # Footer line directly below the separator (cyan, like free models)
        if footer:
            try:
                stdscr.addstr(sep_y + 1, 2, footer[:width - 4], curses.color_pair(P.VALUE))
            except curses.error:
                pass

        # Legend bar
        legend = [("↑/↓", "move"), ("Enter/→", "select")]
        if multi:
            legend.append(("Space", "toggle"))
        if back_on_left:
            legend.append(("←", "back"))
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


def _curses_model_search_win(ids: list[str], models: dict, stdscr) -> bool:
    """curses search widget drawn into an existing stdscr: type to filter
    model ids live, arrow to move, Enter toggles the selected model's
    enabled state, q/ESC finishes. Left/Right arrows page.
    Mutates models in place. Returns True if any toggle happened, False otherwise."""
    curses.set_escdelay(25)
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    _curses_init_colors()
    _curses_theme_bkgd(stdscr)
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
        _curses_theme_bkgd(stdscr)
        
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
                stdscr.addstr(2, 0, "  (no matches)", curses.color_pair(P.MUTED))
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
                    row_pair = P.SELECTED
                elif enabled:
                    row_pair = P.ENABLED
                elif is_free:
                    row_pair = P.FREE
                else:
                    row_pair = P.DISABLED
                stdscr.addstr(2 + row, 0, " " * (width - 1), curses.color_pair(row_pair))
                stdscr.addstr(2 + row, 0, line[: width - 2].ljust(width - 1), curses.color_pair(row_pair))
            except curses.error:
                pass
        
        # Separator after enabled
        sep_idx = enabled_count
        if 0 < enabled_count < len(filtered) and top <= sep_idx - 1 < top + list_h:
            y = 2 + sep_idx - top
            try:
                stdscr.addstr(y, 0, "─" * (width - 1), curses.color_pair(P.CHEVRON))
            except curses.error:
                pass
        
        # Separator after free-disabled
        free_sep_idx = enabled_count + free_disabled_count
        if free_disabled_count > 0 and free_sep_idx < len(filtered) and top <= free_sep_idx - 1 < top + list_h:
            y = 2 + free_sep_idx - top
            try:
                stdscr.addstr(y, 0, "─" * (width - 1), curses.color_pair(P.FREE))
            except curses.error:
                pass
        
        legend = [("↑/↓/←/→", "move"), ("ESC", "back"), ("Enter", "toggle"), ("type", "filter")]
        _curses_draw_legend(stdscr, legend)

        stdscr.refresh()
        _emit_sgr_bg()
        ch = stdscr.getch()
        if ch == curses.KEY_RESIZE:
            _curses_theme_bkgd(stdscr)
            continue
        if ch == 27:  # ESC -> back to provider menu
            return changed
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
                return changed
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
    curses.set_escdelay(25)
    try:
        curses.curs_set(0)
    except curses.error:
        pass
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


def _curses_config_flow(providers_doc: dict, providers: list) -> bool | object:
    """Run the whole --config flow inside ONE curses session so there is no
    terminal-mode flash between menus. Returns True if providers.json
    changed, False if not, or _CURSES_FAILED on any curses error."""
    import curses

    def main(stdscr) -> bool:
        changed = False
        while True:
            if not providers:
                return changed
            ordered = sorted(providers, key=lambda p: p["id"])
            labels = [_provider_label(p) for p in ordered]
            pi = _curses_select_win(
                stdscr, labels, "Select a provider  (q: quit)"
            )
            if pi is None:
                return changed
            selected = ordered[pi]
            while True:
                enabled = bool(selected.get("enabled", True))
                actions = [
                    "Configure models",
                    f"{'Disable' if enabled else 'Enable'} provider",
                    "Delete provider",
                    "Back",
                ]
                ai = _curses_select_win(
                    stdscr,
                    actions,
                    f"Provider: {selected.get('name') or selected['id']}",
                    back_on_left=True,
                    footer=(
                        f"Required env var: {_env_status_line(first_env_key(selected))}"
                        if first_env_key(selected)
                        else None
                    ),
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
                        _record_removed_provider(providers_doc, selected["id"])
                        providers[:] = [
                            p for p in providers if p.get("id") != selected["id"]
                        ]
                        dump_json(PROVIDERS_PATH, providers_doc)
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
    for provider in providers:
        pid = provider["id"]
        penabled = bool(provider.get("enabled", True))
        mm = provider.get("models")
        ids = list(mm.keys()) if isinstance(mm, dict) else []
        enabled_ids = [
            mid
            for mid in ids
            if penabled and isinstance(mm[mid], dict) and mm[mid].get("enabled", True)
        ]
        if not enabled_ids:
            continue
        total_enabled += len(enabled_ids)
        pname = provider.get("name") or pid
        order, _, _ = _sort_model_indices(enabled_ids, mm or {})
        for idx in order:
            mid = enabled_ids[idx]
            m = mm[mid] if isinstance(mm[mid], dict) else {}
            mname = m.get("name") or mid
            print(f"● {mname} ({pname}) - {pid}/{mid}")

    if not total_enabled:
        print("No enabled models. Enable with --enable or --config")
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
    dump_json(PROVIDERS_PATH, providers_doc)
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

    dump_json(PROVIDERS_PATH, providers_doc)
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
            dump_json(PROVIDERS_PATH, providers_doc)
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

        base_url = pinfo.get("api") or ""
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
        dump_json(PROVIDERS_PATH, providers_doc)

    path = write_config_toml(managed_ids, tables)
    return path, stats


def print_sync_report(stats: dict, path: Path, providers_doc: dict) -> None:
    """Sync summary followed by required env vars for enabled providers."""
    print_summary(stats, path)
    print_env_requirements(providers_doc)


def add_provider_entry(providers_doc: dict, api: dict, provider_id: str) -> None:
    """Add provider_id to providers_doc with all models disabled and persist."""
    existing = {p.get("id") for p in providers_doc["providers"] if isinstance(p, dict)}
    if provider_id in existing:
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
    entry["enabled"] = True
    entry["models"] = models_map
    providers_doc["providers"].append(entry)
    dump_json(PROVIDERS_PATH, providers_doc)
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
        ordered = sorted(providers, key=lambda p: p["id"])
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
                    _record_removed_provider(providers_doc, selected["id"])
                    providers[:] = [
                        p for p in providers if p.get("id") != selected["id"]
                    ]
                    dump_json(PROVIDERS_PATH, providers_doc)
                    print(f"Deleted provider {selected['id']!r}.")
                    changed = True
                break
    return changed


def cmd_config() -> int:
    providers_doc = load_providers()
    providers = [p for p in providers_doc["providers"] if isinstance(p, dict) and p.get("id")]
    if not providers:
        print("No providers configured yet. Add with --add-provider")
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
        description="Grok Build config.toml [model.<provider-id>-<model-id>] tables will be added, updated or deleted by this command for any matched pattern of <provider-id>-<model-id>. Uniquely name your manually configured custom models to avoid modification.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  grok-models.py                              sync to config.toml\n"
            "  grok-models.py --providers                  show configured providers\n"
            "  grok-models.py --provider opencode          show models for a provider\n"
            "  grok-models.py --models                     show currently enabled models\n"
            "  grok-models.py --enable opencode            enable providers\n"
            "  grok-models.py --enable opencode/hy3-free   enable model\n"
            "  grok-models.py --disable openrouter         disable a provider\n"
            "  grok-models.py --disable-all                disable all models\n"
        ),
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "--add-provider",
        metavar="ID",
        help="Add provider ID",
    )
    group.add_argument(
        "--search",
        metavar="TERM",
        help="Search providers",
    )
    group.add_argument(
        "--config",
        action="store_true",
        help="Configure a provider or its models",
    )
    group.add_argument(
        "--disable-all",
        action="store_true",
        help="Disable all models in every provider",
    )
    group.add_argument(
        "--disable",
        action="append",
        metavar="TARGET",
        default=[],
        help="Disable TARGET (provider or provider/model); repeatable",
    )
    group.add_argument(
        "--enable",
        action="append",
        metavar="TARGET",
        default=[],
        help="Enable TARGET (provider or provider/model); repeatable",
    )
    group.add_argument(
        "--models",
        action="store_true",
        help="Show enabled models",
    )
    group.add_argument(
        "--providers",
        action="store_true",
        help="Show configured providers",
    )
    group.add_argument(
        "--provider",
        metavar="ID",
        help="Show the models for this provider",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.add_provider:
            return cmd_add_provider(args.add_provider)
        if args.search:
            return cmd_search(args.search)
        if args.config:
            return cmd_config()
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
        return cmd_sync()
    except SyncError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("Interrupted.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
