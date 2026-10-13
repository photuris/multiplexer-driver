//! `notify` through a fake `cmux` on PATH (§9).
#![cfg(unix)]

use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU32, Ordering},
};

/// Distinguishes the directories of tests in one process.
static NEXT: AtomicU32 = AtomicU32::new(0);

/// The fallback method that the fake OS notifier answers as.
const OS_METHOD: &str = if cfg!(target_os = "macos") {
    "osascript"
} else {
    "notify-send"
};

/// A temp directory holding the fake tools; removed on drop.
struct Sandbox {
    /// The directory first on the child's PATH.
    dir: PathBuf,
}

impl Sandbox {
    /// Creates the directory with fake `cmux`, `osascript`, and
    /// `notify-send`.
    fn new() -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "md-notify-cmux-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cmux");

        fs::create_dir_all(&dir)?;
        fs::copy(fixtures.join("fake-cmux.sh"), dir.join("cmux"))?;
        make_executable(&dir.join("cmux"))?;

        for name in ["osascript", "notify-send"] {
            write_script(&dir.join(name), "exit 0")?;
        }

        Ok(Self { dir })
    }

    /// The fake cmux call log.
    fn log(&self) -> PathBuf {
        self.dir.join("log")
    }

    /// Runs `notify` with the given extra environment.
    fn notify(&self, envs: &[(&str, &str)]) -> Output {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cmux");
        let mut command =
            Command::new(env!("CARGO_BIN_EXE_multiplexer-driver"));

        command
            .args(["notify", "--title", "Overseer", "--message", "plan ready"])
            .env("PATH", format!("{}:/usr/bin:/bin", self.dir.display()))
            .env("FAKE_CMUX_LOG", self.log())
            .env("FAKE_CMUX_FIXTURES", fixtures)
            .env_remove("HERDR_ENV")
            .env_remove("RUST_LOG")
            .env_remove("CMUX_SOCKET_PATH")
            .envs(envs.iter().copied());

        command
            .output()
            .unwrap_or_else(|e| panic!("running the binary: {e}"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir); // best effort cleanup
    }
}

/// Marks `path` executable.
fn make_executable(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

/// Writes an executable `/bin/sh` script with the given body.
fn write_script(path: &Path, body: &str) -> io::Result<()> {
    fs::write(path, format!("#!/bin/sh\n{body}\n"))?;

    make_executable(path)
}

/// Stdout as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The `error.type` of the last stderr line.
fn error_type(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().last().unwrap_or_default();
    let value: serde_json::Value =
        serde_json::from_str(last).unwrap_or_default();

    value["error"]["type"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The JSON line `notify` prints for the OS fallback.
fn os_line() -> String {
    format!("{{\"sent\":true,\"method\":\"{OS_METHOD}\",\"reason\":null}}\n")
}

#[cfg(test)]
mod notify {
    use super::*;

    #[test]
    fn should_notify_through_cmux_when_inside_cmux() {
        let sandbox = Sandbox::new().unwrap();
        let output =
            sandbox.notify(&[("CMUX_SOCKET_PATH", "/tmp/md-notify.sock")]);
        let log = fs::read_to_string(sandbox.log()).unwrap_or_default();
        let argv: Vec<&str> =
            log.lines().filter_map(|l| l.strip_prefix('\t')).collect();

        assert_eq!(
            (output.status.code(), stdout(&output).as_str()),
            (
                Some(0),
                "{\"sent\":true,\"method\":\"cmux\",\"reason\":null}\n"
            )
        );
        assert!(log.lines().any(|l| l == "#env CMUX_QUIET=1"), "log {log:?}");
        assert_eq!(
            argv,
            ["notify", "--title", "Overseer", "--body", "plan ready"]
        );
    }

    #[test]
    fn should_fall_back_to_os_notifier_when_socket_variable_absent() {
        let sandbox = Sandbox::new().unwrap();
        let output = sandbox.notify(&[]);

        assert_eq!(
            (
                output.status.code(),
                stdout(&output),
                sandbox.log().exists()
            ),
            (Some(0), os_line(), false)
        );
    }

    #[test]
    fn should_skip_cmux_when_socket_variable_empty() {
        let sandbox = Sandbox::new().unwrap();
        let output = sandbox.notify(&[("CMUX_SOCKET_PATH", "")]);

        assert_eq!(
            (
                output.status.code(),
                stdout(&output),
                sandbox.log().exists()
            ),
            (Some(0), os_line(), false)
        );
    }

    #[test]
    fn should_exit_1_when_cmux_fails() {
        let sandbox = Sandbox::new().unwrap();
        let output = sandbox.notify(&[
            ("CMUX_SOCKET_PATH", "/tmp/md-notify.sock"),
            ("FAKE_CMUX_FAIL", "notify"),
        ]);

        assert_eq!(
            (output.status.code(), error_type(&output).as_str()),
            (Some(1), "unexpected")
        );
    }

    #[test]
    fn should_fall_back_when_cmux_is_not_installed() {
        let sandbox = Sandbox::new().unwrap();

        fs::remove_file(sandbox.dir.join("cmux")).unwrap();

        let output =
            sandbox.notify(&[("CMUX_SOCKET_PATH", "/tmp/md-notify.sock")]);

        assert_eq!(
            (output.status.code(), stdout(&output)),
            (Some(0), os_line())
        );
    }

    #[test]
    fn should_prefer_herdr_when_inside_herdr() {
        let sandbox = Sandbox::new().unwrap();

        write_script(
            &sandbox.dir.join("herdr"),
            "echo '{\"result\":{\"shown\":true}}'",
        )
        .unwrap();

        let output = sandbox.notify(&[
            ("HERDR_ENV", "1"),
            ("CMUX_SOCKET_PATH", "/tmp/md-notify.sock"),
        ]);

        assert_eq!(
            (
                output.status.code(),
                stdout(&output).as_str(),
                sandbox.log().exists()
            ),
            (
                Some(0),
                "{\"sent\":true,\"method\":\"herdr\",\"reason\":null}\n",
                false
            )
        );
    }
}
