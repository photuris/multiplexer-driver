//! Runs harness subprocesses with a hard time ceiling so no call can
//! hang (§4.2).

use std::{
    io::{self, Read},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Longest any single harness subprocess may run.
pub const CEILING: Duration = Duration::from_secs(30);

/// Runs `command` to completion and collects its output, killing it
/// and returning an [`io::ErrorKind::TimedOut`] error past `limit`.
///
/// # Errors
///
/// Spawn failures (e.g. [`io::ErrorKind::NotFound`] for a missing
/// binary), wait failures, and the timeout above.
pub fn output_within(
    command: &mut Command,
    limit: Duration,
) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = thread::spawn(move || read_all(stdout));
    let err_reader = thread::spawn(move || read_all(stderr));
    let deadline = Instant::now() + limit;

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }

        if Instant::now() >= deadline {
            let _ = child.kill(); // already past the ceiling; best effort
            let _ = child.wait(); // reap; the result changes nothing

            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no exit within {limit:?}"),
            ));
        }

        thread::sleep(Duration::from_millis(10));
    };

    Ok(Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    })
}

/// Reads a child pipe to its end; a missing pipe or read error gives
/// what was read so far.
fn read_all(pipe: Option<impl Read>) -> Vec<u8> {
    let mut buffer = Vec::new();

    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buffer); // partial output is still useful
    }

    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    mod output_within {
        use super::*;

        #[test]
        fn should_capture_stdout_when_child_exits() {
            let mut command = Command::new("sh");
            command.args(["-c", "echo hi"]);

            let output = output_within(&mut command, CEILING).unwrap();

            assert!(output.status.success());
            assert_eq!(output.stdout, b"hi\n");
        }

        #[test]
        fn should_kill_when_child_outlives_limit() {
            let mut command = Command::new("sleep");
            command.arg("5");
            let started = Instant::now();

            let err = output_within(&mut command, Duration::from_millis(100))
                .unwrap_err();

            assert!(
                err.kind() == io::ErrorKind::TimedOut
                    && started.elapsed() < Duration::from_secs(2),
                "got {err:?} after {:?}",
                started.elapsed()
            );
        }
    }
}
