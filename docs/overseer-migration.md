# Migrating from overseer-driver

This project does not edit the overseer repository. This page is the
input for that separate change. It maps each `overseer-driver` command
that the overseer skill uses in `resources/tmux.md`, `resources/herdr.md`,
and `resources/judge.md` to its `multiplexer-driver` command.

## Command map

| Old | New |
|---|---|
| `overseer-driver spawn --harness H --name N -- cmd` | `multiplexer-driver --harness H pane spawn --name N -- cmd` |
| `overseer-driver split --harness H --target T --name N --direction D -- cmd` | `multiplexer-driver --harness H pane split T --name N --direction D -- cmd` |
| `overseer-driver read --harness H --target T --lines N [--ansi]` | `multiplexer-driver --harness H pane read T --lines N [--ansi]` |
| `overseer-driver prompt --harness H --target T --text X` | `multiplexer-driver --harness H pane prompt T --text X` |
| `overseer-driver list --harness H` → `{"handles":[…]}` | `multiplexer-driver --harness H pane list` → JSONL `PaneRecord` (`jq -r .handle`) |
| `overseer-driver rename --harness H --target T --label L` | `multiplexer-driver --harness H pane rename T --label L` |
| `overseer-driver interrupt --harness H --target T` | `multiplexer-driver --harness H pane interrupt T` |
| `overseer-driver interrupt --harness H --target T --kill` | `multiplexer-driver --harness H pane kill T` |
| `overseer-driver status --harness H --target T [--patterns F]` | `multiplexer-driver --harness H pane status T [--patterns F]` |
| `overseer-driver notify --message M [--title T] [--sound S]` | `multiplexer-driver notify --message M [--title T] [--sound S]` |

## Behavior changes

Skill authors must know these changes.

- tmux handles are now `%N` pane IDs. They are not
  `session:window.pane`.
- tmux `spawn` opens a window in a session. It does not start a new
  session. Outside tmux, it needs `--workspace`.
- `list` covers every pane, not only agents. To keep only agents, use
  `jq 'select(.agent)'`.
- A failure now ends stderr with a JSON record. Exit codes are typed.
- New commands and flags: `workspace …`, `pane wait`,
  `agent resume-args`, and `--session`.
