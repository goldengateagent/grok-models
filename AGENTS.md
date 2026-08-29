# Agent instructions

## Communication

- If the user asks a question, do not respond by editing code. Answer the
question.

## Git

- Do not propose git operations unless the user asks for them.
- Read-only inspect commands do not need approval. Run them when git work is
requested: `git status`, `git diff`, `git diff --staged`, `git log`. Never treat
a chat-start snapshot as live status.
- Mutating commands (`add`, `rm`, `commit`, `reset`, `checkout`, `restore`,
`push`, `pull`, `merge`, `rebase`, and anything that changes the index or
working tree) require the user to authorize them by saying `exec git`.
- When the user asks for git work, print the exact mutating command(s) for
review before asking for authorization.
- After approval, run only the authorized command(s) and echo the exact
command(s) issued.
- do not git revert any files without authorization

## Project files

- do not delete any files without authorization

## Production isolation

- Never read, write, copy, or quote live user config or secrets into tests,
fixtures, source, commits, or commands. That includes `$GROK_HOME`,
`$CODEX_HOME`, `~/.grok`, `~/.codex`, and every file under them (`config.toml`,
`providers.json`, `*-models.json`, and others). Do not use the process
environment's `<PROVIDER>_API_KEY` values (or other API keys) in test or
production code.
- Before any test run — scripted or in an agent turn — set `GROK_HOME` and
`CODEX_HOME` to empty temp dirs. Do not unset them so they fall back to
`~/.grok` / `~/.codex`.
- Fixtures must be invented: synthetic paths only (`/tmp/proj`), never
`/Users/...`, `/home/...`, usernames, real project dirs, or leftover TOML/JSON
from a developer machine.

