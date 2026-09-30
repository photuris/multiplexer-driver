# multiplexer-driver: design

Status: approved in brainstorming on 2026-09-30. This spec is the single
source of truth for the implementation. It is a clean-room
re-implementation in Rust of `overseer-driver` (Go, commit `d537633`)
plus the gap features in `overseer-driver-gaps.md`. Implementers work
from this document. They do not read the Go source. Every behavior of
the Go version that matters is written down here.

## 1. Purpose

`multiplexer-driver` is a CLI that drives terminal multiplexer
"harnesses" that host coding agents. It gives callers one vocabulary
for workspaces, tabs, panes, and the agents inside them. Callers are
scripts and LLM agents first, humans second.

Current consumers:

- The `overseer` skill (`~/Projects/overseer`). Its
  `resources/*.md` files call the old `overseer-driver` CLI. They need a
  rewrite pass after this ships (see section 11). This project does not
  edit that repo.
- A future project, tentatively `herd`, that takes stock of a harness,
  closes workspaces, and rebuilds them later ("park" and "wake").

Harnesses in scope: **tmux** and **Herdr** (0.8.2). Planned later, out
of scope now: cmux, orca, T3 Code. The design must not block them.

The CLI contract is new. It does not stay compatible with
`overseer-driver`.

## 2. Non-goals

- Layout restore (pane split geometry).
- Event streaming or a daemon.
- Guessing agent conversation IDs on tmux from agent session files
  (deferred, see section 12).
- `--env`, `--env-file`, `--dry-run`, `--input` from the cli-utilities
  skill. There are no credentials and no JSON-bodied writes.
- Herdr's socket protocol. We shell out to the `herdr` CLI (see 4.3).

## 3. Toolchain and conventions

- Rust edition 2024, `rust-version` = output of `rustc --version`
  (1.98 at time of writing). One crate, binary and library.
- Follows the `rust-style`, `coding-principles`, and `cli-utilities`
  skills: `rustfmt.toml` with `max_width = 79`, the lint table and
  `clippy.toml` from `rust-style`, doc comments on every item, no
  `unwrap`/`expect` outside tests, `thiserror` for the one error enum,
  `tracing` for diagnostics on stderr. No `anyhow`: every error is
  already the typed enum that `main.rs` maps to an exit code.
- Synchronous. No `tokio`. Subprocesses run through
  `std::process::Command`.
- Dependencies: `clap` (derive, env), `serde`, `serde_json`, `regex`,
  `thiserror`, `tracing`, `tracing-subscriber` (env-filter).
  Dev: `rstest`, `insta`. Add nothing else without a reason in the
  commit message.
- `Cargo.lock` is committed. CI and local checks use `--locked`.
- Project files: `AGENTS.md`, `CLAUDE.md` (contains only
  `@AGENTS.md`), `README.md`, MIT `LICENSE`.

## 4. Architecture

### 4.1 Layout

```
src/
  main.rs     thin: parse args, init tracing, call run(), map error → exit
  lib.rs      crate docs, module list, run(): dispatch → driver → print
  cli.rs      clap definitions (help text is generated from these)
  model.rs    Handle, WorkspaceRecord, PaneRecord, Status, … (serde)
  driver.rs   Driver trait, Error enum, Direction, StartOutcome
  herdr.rs    Herdr driver
  tmux.rs     tmux driver
  agents.rs   shared agent knowledge: Herdr kinds, resume-args table
  patterns.rs status patterns file: load, compile, classify a tail
  process.rs  run a subprocess with a hard time ceiling
  notify.rs   notify (harness-independent)
  text.rs     trim trailing blank lines, POSIX shell quoting
tests/
  tmux.rs     integration tests against a private tmux server
  herdr.rs    integration tests against a dedicated Herdr test session
  cli.rs      exit codes, output shape, help snapshots
```

A module grows a directory only when it gets a second responsibility.

### 4.2 The `Driver` trait

One trait covers every operation. A harness that cannot do an operation
returns `Error::Unsupported`. There are no optional sub-traits. This
keeps future harnesses (cmux, orca, T3) to "implement the trait, return
`Unsupported` where needed".

Operations (names are indicative, signatures are the implementer's
call within `rust-style`):

- `workspace_list`, `workspace_create(label, cwd)`,
  `workspace_close(id)`, `workspace_tag(id, set, clear)`
- `pane_list(workspace filter)`
- `pane_spawn(name, workspace, cwd, command)`,
  `pane_split(target, direction, name, cwd, command)`
- `pane_read(target, lines, ansi)`, `pane_prompt(target, text)`,
  `pane_rename(target, label)`, `pane_interrupt(target)`,
  `pane_kill(target)`
- `pane_status(target)`, `raw(args)`

`notify` and `agent resume-args` do not go through the trait.

`pane_wait` is one shared function in `driver.rs`, not a per-driver
method: it calls `pane_status` every 500 ms until the status is in the
`--until` set, and fails with `timeout` past the deadline. It works for
every harness that implements `pane_status`. It sleeps no longer than
the time left, and a result that arrives after the deadline is a
timeout even when it matches.

Every harness subprocess runs with a hard 30 s ceiling
(`process.rs`). A call past it is killed and reported as
`harness_unavailable`. So no command can hang, and `pane wait` can
overrun its deadline by at most one bounded status call.

Each driver keeps its subprocess call in one private helper (run, and
run-and-parse-JSON). Argv construction and output parsing live in pure
functions so unit tests cover them without a harness.

### 4.3 Harness access

Each driver shells out to the harness CLI. We chose this over Herdr's
socket protocol because the CLI is the public, documented interface.
If Herdr's socket protocol becomes stable later, only `herdr.rs`
changes: replace its private run helper. Nothing outside that file
depends on how Herdr is reached.

### 4.4 Session selection (gap 4)

The global `--session <name>` flag selects which harness server to
use. Without it, the ambient server is used, as the environment
already selects it.

- **Herdr:** set `HERDR_SOCKET_PATH` on every `herdr` child process.
  Before that, remove every `HERDR_*` variable from the child
  environment, so the caller's own session, workspace, and pane
  variables never leak into a call aimed at another session.
  The path is `$XDG_CONFIG_HOME/herdr/sessions/<name>/herdr.sock`
  (`$XDG_CONFIG_HOME` defaults to `~/.config`). The name `default`
  maps to `$XDG_CONFIG_HOME/herdr/herdr.sock`. If the socket file does
  not exist, fail with `harness_unavailable` (exit 3) and name the path.
- **tmux:** pass `-L <name>` before every tmux subcommand.

The driver must never fall back to a different session than the one
named. A wrong server is worse than a failure.

### 4.5 Agent knowledge (`agents.rs`)

Agent facts are not harness facts. They live in one table used by
every driver.

**Herdr known kinds** (the `--kind` values `herdr agent start`
accepts, from `herdr agent start --help` on 0.8.2): `pi`, `claude`,
`codex`, `gemini`, `cursor`, `devin`, `agy`, `cline`, `omp`,
`mastracode`, `opencode`, `copilot`, `kimi`, `kiro`, `droid`, `amp`,
`grok`, `hermes`, `kilo`, `qodercli`, `qwen`, `maki`. The tmux driver
uses the same list to recognize an agent from a command name.

**Resume arguments** (verified from each tool's `--help` on
2026-09-30):

| Kind     | argv                               | Session ref kind |
|----------|------------------------------------|------------------|
| `claude` | `claude --resume <ref>`            | id               |
| `codex`  | `codex resume <ref>`               | id               |
| `pi`     | `pi --session <ref>`               | path or id       |

Any other kind returns `unsupported` (exit 5). Add rows only after a
live check of that tool's help.

## 5. CLI

### 5.1 Global flags

| Flag                    | Env fallback                  | Notes |
|-------------------------|-------------------------------|-------|
| `--harness <tmux\|herdr>` | `MULTIPLEXER_DRIVER_HARNESS` | Required for driver-backed commands |
| `--session <name>`      | `MULTIPLEXER_DRIVER_SESSION`  | See 4.4 |
| `--pretty`              |                               | Indent single-object output. Lists stay JSONL |
| `--log-level <l>`       | `RUST_LOG`                    | `debug`, `info`, `warn` (default), `error`; stderr only |
| `--version`, `--help`   |                               | stdout, exit 0 |

Flags beat environment variables. `--harness` and `--session` are
accepted on `notify` and `agent` too, but ignored there.

### 5.2 Commands

```
workspace list                                          → JSONL WorkspaceRecord
workspace create --label L [--cwd D]                    → WorkspaceRecord
workspace close <ws>                                    → {"workspace_id":…,"closed":true}
workspace tag <ws> [key=value…] [--clear key]…          → {"workspace_id":…,"tags":{…}}

pane list [--workspace ws]                              → JSONL PaneRecord
pane spawn --name N [--workspace ws] [--cwd D] -- cmd…  → StartResult
pane split <handle> --name N [--direction right|down] [--cwd D] -- cmd…
                                                        → StartResult
pane read <handle> [--lines 30] [--ansi]                → {"handle":…,"output":…}
pane prompt <handle> --text T                           → {"handle":…,"sent":true}
pane rename <handle> --label L                          → {"handle":…,"label":…}
pane interrupt <handle>                                 → {"handle":…,"interrupted":true}
pane kill <handle>                                      → {"handle":…,"killed":true}
pane status <handle> [--patterns file.json]             → StatusResult
pane wait <handle> --timeout DUR [--until S]… [--patterns file.json]
                                                        → StatusResult

agent resume-args --kind K --session-ref R              → {"argv":[…]}
notify --message M [--title Overseer] [--sound S]       → NotifyResult
raw -- <harness args…>                                  → harness stdout, verbatim
```

Rules:

- The primary identifier is positional. Everything else is a flag.
- `--direction` defaults to `right`. `--lines` defaults to 30.
- `--cwd` defaults to the caller's current directory for `spawn`,
  `split`, and `workspace create`.
- `--timeout` takes a duration: an integer with `ms`, `s`, or `m`
  (`500ms`, `60s`, `5m`). It is required. `wait` must never hang. A
  value that overflows is a usage error.
- `--until` repeats. Values: `idle`, `working`, `blocked`. Default:
  `idle` and `blocked`.
- `--text`, `--label`, `--name`, and `--message` must be non-empty.
  An empty value is a usage error.
- `workspace tag` with no `key=value` and no `--clear` is a usage
  error. Keys match `^[A-Za-z0-9_-]+$`.
- `raw` runs the harness binary with the arguments after `--`, with
  session selection applied, and prints its stdout unchanged. A
  nonzero harness exit maps to `unexpected` (exit 1) with the harness
  stderr in the message, never to `not_found` or `usage`. A binary
  that cannot start, a missing session socket, or the time ceiling is
  `harness_unavailable`. `raw`
  is the escape hatch for anything without a subcommand.
- `agent` is a noun for future agent-level commands. `resume-args`
  is its only verb now.

### 5.3 Records

All field names are `snake_case`. Nullable fields are always present
with `null`, never omitted, so `jq` paths are stable.

**WorkspaceRecord**

```json
{"workspace_id":"w1","label":"project-x","focused":false,
 "tags":{"project":"x"}}
```

`tags` is always an object (empty when there are none). On Herdr it
holds every metadata token on the workspace, including tokens other
tools reported.

**PaneRecord** (gap 2)

```json
{"handle":"w1:p2","workspace_id":"w1","workspace_label":"project-x",
 "tab_id":"w1:t2","tab_label":"impl","label":"impl-003",
 "cwd":"/home/u/proj","agent":"claude",
 "agent_session":{"kind":"id","value":"a6b4…"},
 "session_confidence":"native",
 "status":"idle","status_confidence":"native"}
```

- `pane list` covers every pane, shells included. Agent fields are
  `null` when unknown.
- `agent_session.kind` is `id` or `path`.
- `session_confidence`: `native` or `none`.
- `status`: `working`, `idle`, `blocked`, `unknown`.
  `status_confidence`: `native`, `heuristic`, `none`.

**StartResult** (spawn, split)

```json
{"handle":"w1:p5","started":true}
```

Partial-failure rule: if the pane was created but starting the command
in it failed, print `{"handle":…,"started":false}` on stdout and exit
6 with the error on stderr. The caller needs the handle to inspect or
clean up the pane. If no pane was created, print nothing on stdout.

**StatusResult**

```json
{"handle":"%3","status":"idle","confidence":"heuristic","tail":"…"}
```

`tail` is the last 15 lines when the result is `heuristic` or `none`,
and `null` when `native`.

**NotifyResult**

```json
{"sent":true,"method":"herdr","reason":null}
```

`method`: `herdr`, `notify-send`, `osascript`, `none`.

### 5.4 Output contract

- A single result is one JSON document plus `\n`. It is compact unless
  `--pretty`.
- A list is JSON Lines, one record per line, no wrapping array.
- stdout holds data only. Diagnostics go to stderr through `tracing`.
- UTF-8, no BOM, `\n` line endings.

### 5.5 Errors and exit codes

On failure, the last line on stderr is one JSON object:

```json
{"error":{"type":"not_found","message":"pane %9 not found"}}
```

| Code | `type`                | Meaning |
|------|-----------------------|---------|
| 0    |                       | Success |
| 1    | `unexpected`          | Bug, unparseable harness output, anything else |
| 2    | `usage`               | Bad flags, missing argument, bad input file |
| 3    | `harness_unavailable` | Binary not on PATH, server not running, session socket missing |
| 4    | `not_found`           | Target workspace or pane does not exist |
| 5    | `unsupported`         | Operation not available on this harness |
| 6    | `start_failed`        | Pane exists, command did not start (handle on stdout) |
| 7    | `timeout`             | `pane wait` reached its timeout |
| 130  |                       | SIGINT. No handler: the default signal exit gives shells status 130 and prints no record |

`driver::Error` has one variant per `type`. `main.rs` maps variant to
code and record. Classifying a harness error as `not_found` or
`harness_unavailable` uses the harness's own error output (Herdr's
error `code`, tmux's stderr text). Capture the real strings during
implementation and put them in test fixtures. Anything unrecognized is
`unexpected`.

### 5.6 Help

Help comes from the clap definitions. `multiplexer-driver --help`
lists each noun and the exit-code table. Each verb's help lists its
flags, defaults, the output shape in one sentence, and one runnable
example. `insta` snapshots cover the top-level help and each noun's
help.

## 6. Shared text helpers (`text.rs`)

- **Trim trailing blank lines.** Split on `\n`. Drop trailing lines
  that are empty after whitespace trim. Join with `\n`. Terminal
  captures pad output with blank lines when the pane is taller than
  its content. `pane read` output always goes through this.
- **POSIX shell join.** Render an argv as one shell command line. An
  argument that is non-empty and matches `^[A-Za-z0-9_./=-]+$` stays
  as is. Every other argument is wrapped in single quotes, with each
  `'` written as `'\''`. The empty string becomes `''`. Used where a
  harness takes a command as one shell string (`tmux new-window`,
  `tmux split-window`, `herdr pane run`).

## 7. tmux driver

tmux has no concept of an agent. It knows sessions, windows, panes, and
their text. The mapping:

| Model     | tmux     | ID form |
|-----------|----------|---------|
| workspace | session  | `$3`    |
| tab       | window   | `@5`    |
| pane      | pane     | `%12`   |

Handles are tmux's own stable IDs, never `session:window.pane`
addresses. Index-based addresses shift when windows renumber. IDs do
not, and they survive renames.

### 7.1 Stored metadata

tmux user options hold what tmux does not track:

- `@md-label` (pane option, `set-option -p`): the pane label. Pane
  titles are not used for labels, because agents rewrite the terminal
  title constantly.
- `@md-agent` (pane option): the agent kind, set by `spawn`/`split`
  when `command[0]` is a known kind.
- `@md-tag-<key>` (session option): workspace tags.

Verified on tmux 3.7: `set-option -p -t <pane> @md-label x` works and
`#{@md-label}` reads it in `list-panes -F`.

### 7.2 Commands

Every call is `tmux [-L <session>] <subcommand> …`.

- **workspace list:** `list-sessions -F` with
  `#{session_id}`, `#{session_name}`, `#{session_attached}`. `focused`
  is `session_attached > 0`. Tags: `show-options -t <id>` per session,
  keep lines starting `@md-tag-`. "no server running" means an empty
  list, not an error.
- **workspace create:** `new-session -d -s <label> -c <cwd> -P -F
  '#{session_id}'`. A duplicate name is a tmux error. Report it as
  `usage`.
- **workspace close:** `kill-session -t <id>`.
- **workspace tag:** `set-option -t <id> @md-tag-<k> <v>` per pair,
  `set-option -u -t <id> @md-tag-<k>` per `--clear`. Return the
  resulting tags (read back). Read tags by taking the key names from
  `show-options -t <id>` and each value from `show-options -v -t <id>
  @md-tag-<k>`. Plain `show-options` quotes and escapes values
  (verified: `a"b\c d` prints as `"a\"b\\c d"`), so never parse
  values from it.
- **pane list:** `list-panes -a -F` (or `-s -t <ws>` with a filter)
  with a tab-separated format: `#{pane_id}`, `#{session_id}`,
  `#{session_name}`, `#{window_id}`, `#{window_name}`, `#{@md-label}`,
  `#{pane_current_path}`, `#{@md-agent}`, `#{pane_current_command}`.
  `label` is `@md-label` or `null` if empty. `agent` is `@md-agent`,
  else `pane_current_command` when it is a known kind, else `null`.
  `agent_session` is `null` with `session_confidence` `none`. `status`
  is `unknown` with `status_confidence` `none` (listing does not read
  pane text). "no server running" means an empty list.
  Field order puts `#{pane_current_path}` last. tmux cannot escape tabs
  or newlines in formats (verified), so the parser folds extra fields
  back into `cwd` and treats a line without a pane-ID row prefix as a
  continuation of the previous `cwd`. The driver rejects control
  characters in every label and name it writes (`usage`), as tmux
  already does for window names. Ceiling: an `@md-label` with control
  characters written by another tool can garble that one record; the
  pane stays listed.
- **pane spawn:** new window (tab) in the workspace:
  `new-window -d -t <ws> -n <name> -c <cwd> -P -F '#{pane_id}'
  [<shell-joined command>]`. The workspace defaults to the caller's
  session (`display-message -p '#{session_id}'`) when `$TMUX` is set.
  Otherwise `--workspace` is required (usage error). Then set
  `@md-label` to `<name>` and, for a known kind, `@md-agent`. If
  setting the options fails after the window exists, that is
  `start_failed` with the handle.
- **pane split:** `split-window -d -t <target> -h|-v -c <cwd> -P -F
  '#{pane_id}' [<shell-joined command>]`. `right` is `-h`, `down` is
  `-v`. Then set options as in spawn.
- **pane read:** `capture-pane -p -t <h> -S -<lines>`. With `--ansi`,
  add `-e -J`. `-e` keeps escape codes. `-J` keeps trailing spaces and
  joins wrapped lines, which `-e` needs to render right. Note for
  callers (put in help): the reset code that closes a dim run can land
  at the start of the next line. Trim trailing blank lines.
- **pane prompt:** `send-keys -t <h> -l <text>`, then `send-keys -t
  <h> Enter`. Two calls: `-l` sends text literally, so Enter must be
  separate.
- **pane rename:** `set-option -p -t <h> @md-label <label>` and
  `select-pane -t <h> -T <label>` (the title is cosmetic, for humans).
  The handle never changes.
- **pane interrupt:** `send-keys -t <h> C-c`.
- **pane kill:** `kill-pane -t <h>`. Never kill the window or session.
  They can hold sibling panes.
- **pane status:** read the last 15 lines (no ANSI). With no patterns
  loaded, return `unknown` / `none`. Otherwise check `blocked`
  patterns first, then `idle`. A match gives that status with
  `heuristic`. No match gives `working` with `heuristic`. Always
  include `tail`.
- **pane wait:** requires `--patterns` (usage error otherwise), then
  the shared poll loop (4.2).

### 7.3 Patterns file

```json
{"idle": ["\\$\\s*$"], "blocked": ["Trust this folder\\?"]}
```

Both keys optional. Regexes use the `regex` crate syntax and match
anywhere in the tail. An invalid regex or unreadable file is `usage`.
`--patterns` on Herdr is accepted and ignored (Herdr status is
native).

### 7.4 No server

A server that is not running shows up as `no server running on
<socket>` or, on tmux 3.7 when the socket file is absent, `error
connecting to <socket> (No such file or directory)`. Both mean an empty
result for `workspace list` and `pane list`, and `harness_unavailable`
for everything else. Only these two no-server forms give an empty list:
`error connecting to` with another cause (permission denied, say) and a
missing tmux binary stay `harness_unavailable` for lists too. Other captured strings: `can't find pane: %99`,
`can't find session: $99`, `can't find window: …` (all `not_found`),
`duplicate session: ws` (`usage`).

### 7.5 Server-exit race

tmux exits its server when the last session closes. A command right
after that can fail with "server exited unexpectedly". The run helper
retries once after 200 ms on exactly that message. This was verified
against a real tmux server in the Go version. Keep it.

## 8. Herdr driver

Herdr is agent-aware and reports agent state natively. Every `herdr`
command prints JSON. Success output is `{"id":…,"result":{…}}`. A
failure writes `{"error":{"code":…,"message":…}}` to stderr and exits
non-zero. The run helper parses that envelope into
`"<code>: <message>"`, falls back to raw stderr text, and classifies
by `code` (see 5.5 and 12).

Handles are pane IDs (`w1:p2`). They are Herdr's stable public
address, valid for pane and agent commands, and they survive renames.
Workspace IDs look like `w1`, tab IDs like `w1:t2`.

### 8.1 Commands

- **workspace list:** `workspace list` →
  `result.workspaces[]` with `workspace_id`, `label`, `focused`, and
  `tokens` (an object, absent when empty). `tags` is `tokens` or `{}`.
- **workspace create:** `workspace create --label <L> --cwd <D>
  --no-focus` → `result.workspace` (same shape as a `workspace list`
  entry). It also returns `result.root_pane` and `result.tab`, unused.
- **workspace close:** `workspace close <id>`.
- **workspace tag:** `workspace report-metadata <id> --source
  multiplexer-driver --token k=v…` and `--clear-token k` per clear.
  Then return the workspace's `tokens` from `workspace get <id>`
  (`result.workspace.tokens`). Verified live: tokens appear in `get`
  and `list`, `--clear-token` removes one, tokens from other sources
  are merged in, and tokens without `--ttl-ms` were still present after
  20 s. Whether they survive a server restart is unverified.
- **pane list:** `pane list [--workspace <ws>]` → `result.panes[]`
  with `pane_id`, `workspace_id`, `tab_id`, `cwd`, `agent`,
  `agent_session{kind,value}`, `agent_status`. Add labels by joining
  `workspace list` (workspace label) and `tab list --workspace <ws>`
  (tab `label`) for each workspace present. Pane `label` is the pane
  object's `label` field, present only after `pane rename` (verified),
  else `null`.
  When `agent_session` is present, `session_confidence` is `native`,
  else `none`. `status_confidence` is always `native`.
- **pane spawn:** `tab create --workspace <ws> --cwd <D> --label <N>
  --no-focus` → `result.root_pane.pane_id`. The workspace defaults to
  `$HERDR_WORKSPACE_ID`. If that is unset and `--workspace` is absent,
  it is a usage error. Then start the command in the pane (8.2).
- **pane split:** `pane split --pane <h> --direction right|down --cwd
  <D> --no-focus` → `result.pane.pane_id`. Then start the command
  (8.2).
- **pane read:** `pane read <h> --source visible --lines <n>`, plus
  `--format ansi` with `--ansi`. Trim trailing blank lines.
  **`--source visible` is required.** On Herdr 0.8.2, `--lines` with
  the default source (`recent`, or `recent-unwrapped`) silently returns
  empty output. This was found by an 8/8 failure rate in a spawn+read
  stress loop and confirmed live. Do not remove the flag without a
  live re-test.
- **pane prompt:** `agent prompt <h> <text>`. It works only when
  Herdr recognizes an agent in the pane. Otherwise Herdr errors, and
  the error passes through.
- **pane rename:** `pane rename <h> <label>`.
- **pane interrupt:** `agent send-keys <h> ctrl+c`.
- **pane kill:** `pane close <h>`. Never close the tab. It can hold
  sibling panes.
- **pane status:** `pane get <h>` → `result.pane.agent_status`.
  Map `working` → `working`, `idle` and `done` → `idle`, `blocked` →
  `blocked`, anything else → `unknown`. Confidence is `native`, `tail`
  is `null`. `done` only means "the user has not looked yet" in
  Herdr's UI. It has no meaning for an automated caller.
- **pane wait:** the shared poll loop (4.2). Herdr's own `agent
  wait` is not used: its timeout report could not be observed without
  a real agent, and one loop for every harness is less code.

### 8.2 Starting a command in a pane

The pane already exists here, so every failure in this step is
`start_failed` with the handle.

- If `command[0]` is a known kind (4.5): `agent start <name> --kind
  <kind> --pane <pane> [-- <command[1..]>]`. This can time out waiting
  for the agent to be ready (a folder-trust dialog, for example).
- Otherwise: `pane run <pane> <shell-joined command>`, then `pane
  rename <pane> <name>`. The pane then runs a raw process. Herdr may
  still detect a real agent CLI there later from the screen, but the
  driver does not wait for that. `prompt` and native `status` work
  only after detection.
- An empty command is a usage error on Herdr.

## 9. notify (`notify.rs`)

Reaches the user, not an agent. It does not use `--harness`.

1. If `HERDR_ENV=1` and `herdr` is on PATH: `herdr notification show
   <title> --body <message> [--sound <s>]`. Parse
   `result.shown` and `result.reason`. `sent` is `shown`, not the exit
   status. Herdr exits 0 and shows nothing when the user disabled
   notifications (`{"shown":false,"reason":"disabled"}`). A failed
   command is `unexpected`.
2. Else on Linux with `notify-send` on PATH: `notify-send <title>
   <message>` → `method: "notify-send"`.
3. Else on macOS with `osascript`: `display notification <message>
   with title <title>` with both strings escaped as AppleScript string
   literals (backslash and double quote).
4. Else `{"sent":false,"method":"none","reason":null}`, exit 0.
   Nothing available is a normal outcome, not an error.

`--sound` values: `none`, `done`, `request`, validated by the parser
(anything else is a usage error). Only Herdr uses it.

## 10. Testing

- **Unit (beside the code):** argv builders, output parsers, status
  mapping, pattern matching, shell join, blank-line trim, duration
  parsing, resume table, error classification. Parsers use real JSON
  and text captured from live harnesses, stored under
  `tests/fixtures/`. No mock runner trait.
- **CLI (`tests/cli.rs`):** exit code and error record per error type,
  JSONL shape, help snapshots (`insta`), no BOM and no `\r` in output.
- **tmux integration (`tests/tmux.rs`):** each test starts a private
  server with `-L md-test-<pid>-<n>`. At the end it kills the server
  and removes the socket file, which tmux leaves behind (verified).
  Tests never touch the user's default tmux server. They skip only
  when the `tmux` binary is not installed. Any other startup failure
  fails the test.
- **Herdr integration (`tests/herdr.rs`):** each test binary starts
  its own headless server, `herdr --session md-test-<pid> server`, in
  the background, with every `HERDR_*` variable removed from its
  environment. It waits for
  `$XDG_CONFIG_HOME/herdr/sessions/md-test-<pid>/herdr.sock` to appear
  (up to 5 s), runs the tests with `--session md-test-<pid>`, then runs
  `herdr server stop` against that socket and `herdr session delete
  md-test-<pid>`. Verified live on 2026-09-30. Tests never touch any
  other session and start only shells, never a paid agent CLI. Skip
  when `herdr` is not on PATH.
- **Checks before every commit:** `cargo fmt --check`, `cargo clippy
  --all-targets --all-features --locked -- -D warnings`, `cargo test
  --locked`.

## 11. Delivery phases

Each phase ends with its tests green.

1. **Scaffold.** Cargo project, lint config, `model.rs`, `driver.rs`
   error enum and exit mapping, `text.rs`, `cli.rs` with every command
   defined, help snapshots. Unimplemented commands return
   `unsupported`.
2. **tmux parity.** `pane spawn/split/read/prompt/rename/interrupt/
   kill/status`, `pane list`, `--session` for tmux.
3. **Herdr parity and notify.** The same set on Herdr, `notify`,
   `--session` for Herdr, `raw` for both.
4. **Gap features.** `workspace list/create/close/tag` on both,
   the full PaneRecord join on Herdr, `pane wait` on both,
   `agent resume-args`.
5. **Docs.** README (quickstart, exit-code table, both harnesses,
   jq and PowerShell examples), AGENTS.md (design decisions and the
   verified quirks in sections 7 and 8), and `docs/overseer-migration.md`:
   the old-to-new command map the overseer skill's `resources/*.md`
   need.

## 12. Open items

- **tmux conversation IDs.** Deferred by decision. Revisit later. A
  lead that needs no guessing: when the driver itself starts an agent,
  it can choose the ID up front (`claude --session-id <uuid>`, `pi
  --session-id <id>`), and store it in a pane option
  (`@md-agent-session`). Codex has no such flag as of 2026-09-30.
- **Herdr error codes** seen live (all on stderr, exit 1):
  `pane_not_found`, `workspace_not_found`, `agent_not_found` (also
  returned by `agent prompt` on a pane with no recognized agent). All
  three map to `not_found`. Other codes are `unexpected` until seen.
