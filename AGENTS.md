# AGENTS.md

`multiplexer-driver` is a Rust CLI that drives tmux, Herdr, cmux, tuios,
and hc. See `README.md` for use. The design is in
`docs/superpowers/specs/2026-09-30-multiplexer-driver-design.md`.

## Conventions

- Binary and crate name: `multiplexer-driver`. Edition 2024, Rust 1.98.
- Dependencies: `clap`, `serde`, `serde_json`, `regex`, `thiserror`,
  `tracing`, `tracing-subscriber`. Dev: `rstest`, `insta`. Do not add
  another without a reason in the commit message.
- Set `max_width = 79` in `rustfmt.toml`. Do not use `unwrap`, `expect`,
  or `panic!` outside tests.
- Give every item a doc comment, private items too. Test modules and
  test functions are the exception: their `should_…` names document
  them. Add `# Errors` to every public function that returns `Result`.
- Put a blank line before every control statement, `return`, and tail
  expression, unless it is first in its block.
- Name tests `should_<expected>_when_<state>` inside `mod <unit>`.
- Before each commit, run `cargo fmt --check`,
  `cargo clippy --all-targets --all-features --locked -- -D warnings`,
  and `cargo test --locked`. All must pass.
- Write commit messages in lower case, imperative mood, on one line.
  Do not use emoji, dashes, or attribution lines.
- Tests never touch your own tmux server or a Herdr session that the
  test did not create.
- Never start a paid agent CLI (claude, codex, pi) in a test. Use `bash`
  or `sleep`.

## Design decisions worth knowing before changing things

- `pane prompt` does not wait. `pane wait` is a separate command. It
  requires a timeout and refuses to run without a status signal. A
  prompt that blocks can hang a caller.
- Status degrades honestly. tmux without patterns reports
  `unknown` with confidence `none`. A guessed status misleads callers.
- Handles are stable harness IDs. They survive a rename. Index-based
  addresses change when windows renumber.
- `pane kill` closes one pane. It never closes the tab or workspace.
  Callers must not lose sibling panes. On cmux the last surface of a
  workspace cannot close alone, so `pane kill` closes the
  then-empty workspace with it; no sibling is lost.
- tmux labels live in `@md-label`, not in the pane title. Agents
  rewrite the title all the time.
- Herdr `pane read` needs `--source visible`. Herdr 0.8.2 fails without
  it (8 of 8 calls failed; confirmed live). Test again before you remove
  the flag.
- Herdr `agent start` knows only a fixed set of kinds. Other kinds fall
  back to `pane run`. They lose native prompt and status until Herdr
  detects an agent from the screen.
- tmux can exit its server when the last session closes. The next
  command can then fail with "server exited unexpectedly". The run
  helper retries once after 200 ms on that message.
- `notify` trusts the `shown` field from Herdr, not its exit code.
- Herdr tags are `tokens`. They are readable and merged across sources.
- Wait is one poll loop for all harnesses. It does not use Herdr
  `agent wait`, so behavior is the same on every harness.
- tmux and Herdr integration tests use private servers only: tmux `-L`,
  and a Herdr session named `md-test-<pid>`. The cmux test has no
  private server. It uses isolated `md-test-<pid>-<nanos>` workspaces in
  the shared app, and a failed snapshot of the preexisting workspaces
  panics it before it creates anything.
- cmux handle = surface UUID, because `surface:N` refs are positional
  and shift when something closes.
- cmux `pane spawn` creates a workspace, one per agent, so every agent
  stays visible in the sidebar.
- `new-split` in cmux 0.64.22 has no `--command`, so the driver pastes
  the line through `rpc terminal.paste`.
- cmux `rename-tab` and `close-surface` need `--workspace`, which the
  driver finds through `tree --all`.
- cmux refuses to close the last surface of a workspace, so `pane kill`
  closes the workspace then.
- cmux status comes from `cmux sessions --json --surface`. `needsInput`
  also fires for an agent idle for about a minute, so `blocked` on cmux
  means "read the pane".
- `cmux send` rewrites `\n` and `\t`, so prompt text goes through the
  paste method.
- `CMUX_QUIET=1` silences cmux deprecation notices.
- The cmux integration test skips unless `cmux ping` answers. From
  outside a cmux terminal that needs the password socket mode.
- tuios handle = window UUID. `--session` is the tuios session name (`-s`);
  the daemon comes from `XDG_RUNTIME_DIR`, which the driver never sets.
- tuios workspaces are the nine numbered slots of a session. `workspace
  create` and `workspace tag` are unsupported; `pane spawn --workspace`
  is a usage error.
- tuios `pane spawn` opens a window with no command, then types the
  `shell_join` line with `send-text` and presses Enter.
- tuios `pane split` needs an attached client and tiling on, or tuios
  refuses; the driver passes the refusal through as `Unsupported` and
  never enables tiling. The split inherits the target's directory, so
  the request's cwd is ignored.
- tuios `capture-pane` needs `--scrollback` for `--lines N`; without it
  tuios reads only the visible screen.
- tuios `raw` passes its arguments unchanged and does not add `-s`.
- tuios status comes from `get-agent-state`: `working`, `idle`/`done`,
  and `needs_input`/`errored` (blocked); `none` and `unknown` fall back
  to `--patterns`.
- The tuios integration test starts a daemon in `/tmp/tp.XXXXXX` (the
  socket path limit rules out a deeper directory) and sets `TERM` for
  its own attached client, because tuios refuses `TERM=dumb`. It needs
  `script` and skips when `tuios` is missing.
- hc handle = terminal address (`t7`), the stable id hc prints on spawn.
  `--session` becomes `HC_SESSION`; with a session the driver also
  removes every other `HC_*` variable from the child, so the caller's
  own terminal context never leaks in.
- hc has two names for a pane. `--name` is hc's unique name (1 to 32 of
  `a-z 0-9 - _ .`, starting with a letter or digit); `md.label` is our
  label and always set. `pane rename` changes only `md.label`, because
  hc cannot rename. hc rejects id-shaped names (`t7`) with
  `invalid_params` in a shape it does not document, so the driver
  retries the spawn without `--name` instead of guessing the rule.
- hc workspace = a herdr-app space id, stored as the `herdr.space`
  label. `pane spawn --workspace` writes it; `--workspace` is optional.
  The driver reads `herdr.space` for the workspace and `herdr.tab` for
  the tab, and never writes `herdr.tab`.
- hc `pane kill` is `hc kill` then `hc rm`. A terminal that already
  finished makes `kill` fail with `finished`; the driver ignores that
  and still removes it.
- hc `pane split` and every `workspace` command are unsupported by
  decision: the herdr app owns layout. `pane read --ansi` is
  unsupported because hc keeps no escape codes.
- hc status is native from the program's OSC 7501 report (`working`,
  `idle`/`done`, `blocked`/`error`). A live terminal that reported
  nothing falls back to `--patterns`, else `unknown`. `resolve_status`
  now lives in `patterns.rs`, shared by tuios and hc.
- hc `pane spawn` passes `--outlive-parent` so the terminal survives
  the process that started it.
- The hc integration test starts two private cores (the one under test
  and a sentinel) with `HC_SESSION` and scratch `XDG_RUNTIME_DIR`,
  `XDG_STATE_HOME`, and `XDG_CONFIG_HOME` in `/tmp/hc.XXXXXX`. It
  removes every `HC_*` variable first, so your own core (session
  `default`) is never reached, and it skips when `hc` is missing.
- Open item: tmux conversation IDs are not done (spec section 12).
