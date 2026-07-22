use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::Path,
    thread,
};

#[derive(Debug)]
pub struct CaptureResult {
    pub output: Vec<u8>,
    pub exit_code: u32,
    pub success: bool,
}

/// Runs a command in the platform-native PTY and mirrors its combined terminal
/// output to the parent process. stdout and stderr are intentionally combined,
/// matching terminal ordering as observed by the user.
pub fn run_pty(command: &[String], cwd: &Path) -> Result<CaptureResult> {
    if command.is_empty() {
        bail!("command cannot be empty");
    }
    let pair = native_pty_system().openpty(PtySize {
        rows: terminal_dimension("LINES", 24),
        cols: terminal_dimension("COLUMNS", 80),
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut builder = CommandBuilder::new(&command[0]);
    builder.args(&command[1..]);
    builder.cwd(cwd);

    let mut child = pair
        .slave
        .spawn_command(builder)
        .with_context(|| format!("start {} in a PTY", command[0]))?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let output_thread = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut terminal = std::io::stdout().lock();
        let mut output = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    terminal.write_all(&buffer[..read])?;
                    terminal.flush()?;
                    output.extend_from_slice(&buffer[..read]);
                }
                // PTY masters commonly report EIO rather than EOF after the
                // slave closes. At this point the captured bytes are complete.
                Err(_) => break,
            }
        }
        Ok(output)
    });

    // Forwarding is detached because reads from an interactive stdin cannot be
    // cancelled portably. Closing the PTY after child exit causes writes to end.
    if std::io::stdin().is_terminal() {
        let mut writer = pair.master.take_writer()?;
        thread::spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut writer);
        });
    }

    let status = child.wait()?;
    drop(pair.master);
    let output = output_thread
        .join()
        .map_err(|_| anyhow::anyhow!("PTY output reader panicked"))??;
    Ok(CaptureResult {
        output,
        exit_code: status.exit_code(),
        success: status.success(),
    })
}

fn terminal_dimension(name: &str, fallback: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

use std::io::IsTerminal;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn captures_combined_terminal_output_and_exit_code() -> Result<()> {
        let result = run_pty(
            &["sh".into(), "-c".into(), "printf hello; exit 7".into()],
            Path::new("."),
        )?;
        assert!(String::from_utf8_lossy(&result.output).contains("hello"));
        assert_eq!(result.exit_code, 7);
        assert!(!result.success);
        Ok(())
    }
}
