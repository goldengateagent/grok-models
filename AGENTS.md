# Agent instructions

## Communication

- If the user asks a question, do not respond by editing code. Answer the question.

## Git

- Do not propose git operations unless the user asks for them.
- Read-only inspect commands do not need approval. Run them when git work is requested: `git status`, `git diff`, `git diff --staged`, `git log`. Never treat a chat-start snapshot as live status.
- Mutating commands (`add`, `rm`, `commit`, `reset`, `checkout`, `restore`, `push`, `pull`, `merge`, `rebase`, and anything that changes the index or working tree) require the user to authorize them by saying `exec git`.
- When the user asks for git work, print the exact mutating command(s) for review before asking for authorization.
- After approval, run only the authorized command(s) and echo the exact command(s) issued.

## Project files

- do not delete any files without authorization
