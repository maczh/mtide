//! Process lifecycle and line-framed JSON-RPC primitives.

use serde_json::Value;
use std::{
    io::{self, BufRead, Write},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProcError {
    #[error("process I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("process did not exit before timeout")]
    Timeout,
    #[error("invalid JSON-RPC header")]
    InvalidHeader,
    #[error("JSON-RPC message exceeds the 64 MiB safety limit")]
    MessageTooLarge,
    #[error("invalid JSON-RPC body: {0}")]
    Json(#[from] serde_json::Error),
}

/// Owns a child process. Dropping it always attempts to terminate and reap it.
pub struct ManagedProcess {
    child: Child,
}

impl ManagedProcess {
    pub fn spawn(program: &str, args: &[&str]) -> Result<Self, ProcError> {
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Self { child })
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, ProcError> {
        Ok(self.child.try_wait()?)
    }

    pub fn wait_timeout(&mut self, timeout: Duration) -> Result<ExitStatus, ProcError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(ProcError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn kill(&mut self) -> Result<(), ProcError> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        self.child.kill().map_err(ProcError::Io)
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn write_message<W: Write>(writer: &mut W, message: &Value) -> Result<(), ProcError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > 64 * 1024 * 1024 {
        return Err(ProcError::MessageTooLarge);
    }
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

pub fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Value>, ProcError> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if line
            .get(.."Content-Length:".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Content-Length:"))
        {
            let value = &line["Content-Length:".len()..];
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| ProcError::InvalidHeader)?,
            );
        }
    }
    let length = length.ok_or(ProcError::InvalidHeader)?;
    if length > 64 * 1024 * 1024 {
        return Err(ProcError::MessageTooLarge);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn fr_t02_json_rpc_handles_partial_buffered_messages() {
        let mut bytes = Vec::new();
        write_message(
            &mut bytes,
            &serde_json::json!({"id": 1, "method": "initialize"}),
        )
        .expect("write message");
        let mut reader = Cursor::new(bytes);
        let message = read_message(&mut reader)
            .expect("read message")
            .expect("message");
        assert_eq!(message["method"], "initialize");
    }

    #[test]
    fn fr_t02_drop_reaps_short_lived_process() {
        let mut process = ManagedProcess::spawn("sh", &["-c", "exit 0"]).expect("spawn");
        assert!(process
            .wait_timeout(Duration::from_secs(1))
            .expect("wait")
            .success());
    }

    #[test]
    fn fr_t02_kill_is_idempotent_after_exit() {
        let mut process = ManagedProcess::spawn("sh", &["-c", "exit 0"]).expect("spawn");
        process.wait_timeout(Duration::from_secs(1)).expect("wait");
        process.kill().expect("already exited is not an error");
    }

    #[test]
    fn fr_t02_json_rpc_accepts_header_case_and_rejects_oversized_messages() {
        let mut reader = Cursor::new(b"content-length: 2\r\n\r\n{}".to_vec());
        assert_eq!(
            read_message(&mut reader).expect("read lowercase header"),
            Some(serde_json::json!({}))
        );
        let mut oversized = Cursor::new(b"Content-Length: 67108865\r\n\r\n".to_vec());
        assert!(matches!(
            read_message(&mut oversized),
            Err(ProcError::MessageTooLarge)
        ));
    }
}
