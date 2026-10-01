# multiplexer-driver

`multiplexer-driver` is a command-line tool that controls tmux and Herdr.
It gives scripts and coding agents one set of commands for workspaces,
panes, and the agents that run in them. Every result is JSON.

## Install

Prebuilt binaries are fully static on Linux (musl) and exist for Linux
and macOS on x86_64 and aarch64, and for Windows on x86_64. Install the
latest release on Linux or macOS with:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/photuris/multiplexer-driver/releases/latest/download/multiplexer-driver-installer.sh | sh
```

On Windows, run this in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/photuris/multiplexer-driver/releases/latest/download/multiplexer-driver-installer.ps1 | iex"
```

The installer puts the binary in `~/.local/bin`
(`%USERPROFILE%\.local\bin` on Windows) and adds it to your `PATH` if
needed. Pass `--no-modify-path` to leave your shell profile alone.

Or download an archive from the
[releases page](https://github.com/photuris/multiplexer-driver/releases/latest)
and check it against `sha256.sum`.

To build from source (Rust 1.98 or later):

```bash
cargo install --git https://github.com/photuris/multiplexer-driver --locked
```

## Releases

Tag a version (`git tag v0.2.0 && git push origin v0.2.0`) after you
bump `version` in `Cargo.toml`. The `dist` workflow in
`.github/workflows/release.yml` builds every target and publishes the
release. A pull request builds the same artifacts without publishing
them.

## Quickstart

These commands use a tmux server that has the socket name `md-demo`.

```bash
export MULTIPLEXER_DRIVER_HARNESS=tmux MULTIPLEXER_DRIVER_SESSION=md-demo
tmux -L md-demo new-session -d -s demo -c /tmp
WS=$(multiplexer-driver workspace list | head -1 | jq -r .workspace_id)
H=$(multiplexer-driver pane spawn --name demo-1 --workspace "$WS" \
  --cwd /tmp -- bash | jq -r .handle)
multiplexer-driver pane prompt "$H" --text 'echo hello'
multiplexer-driver pane read "$H" --lines 5
multiplexer-driver pane kill "$H"
```

## Commands

```text
workspace list | create | close | tag
pane      list | spawn | split | read | prompt | rename | interrupt |
          kill | status | wait
agent     resume-args
notify
raw
```

Run `multiplexer-driver <noun> --help` for the flags, defaults, and one
example of each command.

Global flags:

| Flag | Environment variable | Meaning |
|---|---|---|
| `--harness <tmux\|herdr>` | `MULTIPLEXER_DRIVER_HARNESS` | Harness to drive. Required for `workspace`, `pane`, and `raw`. |
| `--session <name>` | `MULTIPLEXER_DRIVER_SESSION` | A Herdr session name or a tmux `-L` socket name. |
| `--pretty` | | Indent single-object output. Lists stay JSON Lines. |
| `--log-level <level>` | `RUST_LOG` | `debug`, `info`, `warn` (default), or `error`. |

A flag overrides its environment variable.

## Output contract

- A single result is one JSON document followed by a newline. The
  document is compact unless you pass `--pretty`.
- A list is JSON Lines: one record per line, with no wrapping array.
- stdout holds data only. Diagnostics go to stderr.
- Output is UTF-8 with no byte order mark and `\n` line endings.
- Field names use `snake_case`. A nullable field is always present with
  the value `null`.
- On failure, the last line on stderr is one JSON object:
  `{"error":{"type":"not_found","message":"pane %9 not found"}}`.

## Exit codes

| Code | `type` | Meaning |
|------|--------|---------|
| 0 | | Success |
| 1 | `unexpected` | Bug, unparseable harness output, anything else |
| 2 | `usage` | Bad flags, missing argument, bad input file |
| 3 | `harness_unavailable` | Binary not on PATH, server not running, session socket missing |
| 4 | `not_found` | Target workspace or pane does not exist |
| 5 | `unsupported` | Operation not available on this harness |
| 6 | `start_failed` | Pane exists, command did not start (handle on stdout) |
| 7 | `timeout` | `pane wait` reached its timeout |
| 130 | | SIGINT. The shell reports 130. No record is printed. |

## Harness notes

**tmux.** A handle is a tmux pane ID such as `%12`. The tool stores
labels and agent kinds in tmux user options that start with `@md-`.
tmux does not know what an agent is doing. `pane status` and
`pane wait` need `--patterns` to report a status. Without patterns, the
status is `unknown` and the confidence is `none`.

**Herdr.** A handle is a Herdr pane ID such as `w1:p2`. Use `--session`
to select a Herdr session. Herdr reports status natively, so the tool
ignores `--patterns`.

## Windows

Windows supports Herdr only. tmux does not run natively on Windows, so
`--harness tmux` fails with `harness_unavailable`. `pane run` quotes
non-agent commands for PowerShell panes. A pane that runs `cmd.exe` will
not parse them. The quoting rejects an argument that is empty, contains
a double quote, or ends with a backslash, because Windows PowerShell 5.1
mangles them. Run `docs/windows-smoke.ps1` inside Herdr to check an
install.

## PowerShell

```powershell
multiplexer-driver --harness herdr pane list | % { $_ | ConvertFrom-Json }
if ($LASTEXITCODE -ne 0) { "failed with exit code $LASTEXITCODE" }
```

## Raw access

Use `raw` for any harness command that has no subcommand here. The tool
applies `--session` and prints the harness stdout unchanged.

```bash
multiplexer-driver --harness herdr raw -- tab list --workspace w1
```

## Development

Run these checks before each commit. All must pass.

```bash
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
```

Integration tests use private servers only. They never touch your own
tmux server or a Herdr session that they did not create.

## License

MIT. See `LICENSE`.
