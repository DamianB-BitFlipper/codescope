//! Terminal-restoration proof (validation list, first item): start the binary in a pty,
//! make it panic, and assert the leave-alternate-screen sequence is emitted and the process
//! exits non-zero — the user's terminal is not corrupted.

#![cfg(unix)]

use std::io::Read;

#[test]
fn terminal_is_restored_on_panic() {
    // portable-pty drives the binary in a real pseudo-terminal.
    let binary = env!("CARGO_BIN_EXE_codescope");
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = portable_pty::CommandBuilder::new(binary);
    cmd.arg("--no-ai");
    cmd.env("CODESCOPE_TEST_PANIC", "1");
    cmd.env("TERM", "xterm-256color");
    cmd.cwd("/tmp");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn codescope");

    // Read whatever the pty master produces until EOF (child exit closes it).
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if std::time::Instant::now() > deadline {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
        if let Ok(Some(_)) = child.try_wait() {
            // drain whatever remains
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 { break; }
                output.extend_from_slice(&buf[..n]);
            }
            break;
        }
    }

    let status = child.wait().expect("wait");
    let text = String::from_utf8_lossy(&output);
    // The terminal must enter and then LEAVE the alternate screen even on panic.
    assert!(text.contains("\u{1b}[?1049h"), "should enter alt screen; got: {text:?}");
    assert!(
        text.contains("\u{1b}[?1049l"),
        "must leave alt screen on panic; got: {text:?}"
    );
    assert!(!status.success(), "panicking binary must exit non-zero");
}
