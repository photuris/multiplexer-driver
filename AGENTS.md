# AGENTS.md

`multiplexer-driver` is a Rust CLI that drives tmux and Herdr. See
`README.md` for use. The design is in
`docs/superpowers/specs/2026-09-30-multiplexer-driver-design.md`.

## Conventions

- Binary and crate name: `multiplexer-driver`. Edition 2024, Rust 1.98.
- Dependencies: `clap`, `serde`, `serde_json`, `regex`, `thiserror`,
  `tracing`, `tracing-subscriber`. Dev: `rstest`, `insta`. Do not add
  another without a reason in the commit message.
- Set `max_width = 79` in `rustfmt.toml`. Do not use `unwrap`, `expect`,
  or `panic!` outside tests.
- Give every item a doc comment, private items too. Add `# Errors` to
  every public function that returns `Result`.
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
  Callers must not lose sibling panes.
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
- Integration tests use private servers only: tmux `-L`, and a Herdr
  session named `md-test-<pid>`.
- Open item: tmux conversation IDs are not done (spec section 12).
