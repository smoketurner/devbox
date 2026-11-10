//! The claimant's terminal during `sandbox attach`: its size, raw mode, and
//! piping it to the session.
//!
//! Terminal modes are set with `stty` on `/dev/tty`, so no `unsafe` termios
//! calls are needed.

use std::fs::File;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// The controlling terminal, when there is one.
pub(crate) struct Tty;

impl Tty {
    /// The controlling terminal, or `None` when not attached to one.
    pub(crate) fn open() -> Option<Self> {
        File::open("/dev/tty").ok().map(|_| Self)
    }

    /// Rows and columns, from `stty size`.
    pub(crate) fn size(&self) -> Option<(u16, u16)> {
        let out = stty(&["size"]).ok()?;
        parse_size(&out)
    }

    /// Put the terminal in raw mode until the returned guard is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal's modes can't be read or set.
    pub(crate) fn raw(&self) -> Result<RawGuard> {
        let saved = stty(&["-g"])?;
        stty(&["raw", "-echo"])?;
        Ok(RawGuard { saved })
    }
}

/// Restores the terminal's saved modes when dropped.
pub(crate) struct RawGuard {
    saved: String,
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if stty(&[self.saved.trim()]).is_err() {
            eprintln!("could not restore the terminal; run `stty sane`");
        }
    }
}

/// Run `stty <args>` against `/dev/tty`; returns its trimmed output.
fn stty(args: &[&str]) -> Result<String> {
    let tty = File::open("/dev/tty").context("open /dev/tty")?;
    let out = Command::new("stty")
        .args(args)
        .stdin(Stdio::from(tty))
        .stderr(Stdio::null())
        .output()
        .context("run stty")?;
    anyhow::ensure!(out.status.success(), "stty {} failed", args.join(" "));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Parse `stty size` output (`rows cols`).
pub(crate) fn parse_size(out: &str) -> Option<(u16, u16)> {
    let mut words = out.split_whitespace();
    let rows = words.next()?.parse().ok().filter(|r: &u16| *r > 0)?;
    let cols = words.next()?.parse().ok().filter(|c: &u16| *c > 0)?;
    Some((rows, cols))
}

/// Pipe stdin to `conn` and `conn` to stdout until the session closes.
///
/// stdin is read on a plain thread, not tokio's blocking pool, so a read still
/// waiting for a key can't keep the process alive after the session ends.
///
/// # Errors
///
/// Returns an error if the session output can't be written.
pub(crate) async fn pipe(conn: UnixStream) -> Result<()> {
    let (mut from_session, mut to_session) = conn.into_split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            let Some(chunk) = buf.get(..n).filter(|c| !c.is_empty()) else {
                break;
            };
            if tx.blocking_send(chunk.to_vec()).is_err() {
                break;
            }
        }
    });
    let input = tokio::spawn(async move {
        while let Some(chunk) = rx.recv().await {
            if to_session.write_all(&chunk).await.is_err() {
                break;
            }
        }
        to_session.shutdown().await.ok();
    });

    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 4096];
    loop {
        let n = from_session
            .read(&mut buf)
            .await
            .context("read the session")?;
        let Some(chunk) = buf.get(..n).filter(|c| !c.is_empty()) else {
            break;
        };
        stdout.write_all(chunk).await.context("write the session")?;
        stdout.flush().await.context("write the session")?;
    }
    input.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn stty_size_parses_rows_then_columns() {
        assert_eq!(parse_size("40 120\n"), Some((40, 120)));
        assert_eq!(parse_size("0 0"), None);
        assert_eq!(parse_size("40"), None);
        assert_eq!(parse_size(""), None);
    }
}
