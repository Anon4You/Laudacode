//! Long-lived process management for the agent.
//!
//! Unlike `run_command` (one-shot, 180 s hard timeout), a process started
//! here keeps running across tool calls: start once, poll output, feed
//! stdin, then stop. Dev servers, watchers and REPL-style programs become
//! usable. Not a PTY — full-screen TUIs won't render, line protocols will.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::process::Child;

const OUT_LIMIT: usize = 8 * 1024; // per-stream retained output
const MAX_PROCS: usize = 16;

/// Shared bounded output buffer fed by a background reader task.
#[derive(Clone, Default)]
struct OutBuf(Arc<Mutex<Vec<u8>>>);

impl OutBuf {
    fn push(&self, chunk: &[u8]) {
        let mut b = self.0.lock().unwrap();
        b.extend_from_slice(chunk);
        if b.len() > OUT_LIMIT {
            let excess = b.len() - OUT_LIMIT;
            b.drain(..excess);
        }
    }
    fn snapshot(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

/// One managed child process.
pub struct Process {
    pub command: String,
    child: Child,
    readers: Vec<tokio::task::JoinHandle<()>>,
    stdout_buf: OutBuf,
    stderr_buf: OutBuf,
}

impl Process {
    /// Spawn `sh -c command`, piped stdio, own process group (unix) so
    /// kill() takes descendants down too.
    fn spawn(command: &str, cwd: &Path) -> Result<Self> {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting process: {command}"))?;
        let stdout_buf = OutBuf::default();
        let stderr_buf = OutBuf::default();
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            let buf = stdout_buf.clone();
            readers.push(tokio::spawn(drain_pipe(out, buf)));
        }
        if let Some(err) = child.stderr.take() {
            let buf = stderr_buf.clone();
            readers.push(tokio::spawn(drain_pipe(err, buf)));
        }
        Ok(Self {
            command: command.to_string(),
            child,
            readers,
            stdout_buf,
            stderr_buf,
        })
    }

    /// Status + retained output tails. Polling never signals a process.
    async fn status(&mut self) -> String {
        // Keep the group leader unreaped until stop/drop so its PID cannot
        // be reused while descendants still belong to this process group.
        let exited = self.exited_unreaped();
        let mut report = String::new();
        match exited {
            Ok(Some(code)) => report.push_str(&format!("[status: exited, code {code}]\n")),
            Ok(None) => report.push_str("[status: running]\n"),
            Err(e) => report.push_str(&format!("[status unavailable: {e}]\n")),
        }
        report.push_str("[latest output: at most 8 KiB per stream; older bytes discarded]\n");
        let out_snap = self.stdout_buf.snapshot();
        let err_snap = self.stderr_buf.snapshot();
        let so = String::from_utf8_lossy(&out_snap);
        let se = String::from_utf8_lossy(&err_snap);
        if !so.trim().is_empty() {
            report.push_str("--- stdout ---\n");
            report.push_str(&so);
            report.push('\n');
        }
        if !se.trim().is_empty() {
            report.push_str("--- stderr ---\n");
            report.push_str(&se);
            report.push('\n');
        }
        if so.trim().is_empty() && se.trim().is_empty() {
            report.push_str("(no output yet)\n");
        }
        report
    }

    /// Write stdin; `eof=true` closes stdin ("done writing" for REPLs).
    async fn write_stdin(&mut self, input: &str, eof: bool) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        anyhow::ensure!(input.len() <= OUT_LIMIT, "stdin input exceeds 8 KiB");
        anyhow::ensure!(!eof || input.is_empty(), "EOF requires empty input");
        if eof {
            self.child.stdin.take();
            return Ok(());
        }
        let sin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("stdin is closed"))?;
        sin.write_all(input.as_bytes())
            .await
            .context("writing to process stdin")?;
        if !input.ends_with('\n') {
            sin.write_all(b"\n").await.context("writing newline")?;
        }
        sin.flush().await.context("flushing process stdin")?;
        Ok(())
    }

    /// Kill the whole group (unix) and reap. Returns last buffered output.
    async fn kill(&mut self) -> String {
        self.signal_group();
        let _ = self.child.kill().await;
        let code = self
            .child
            .wait()
            .await
            .ok()
            .and_then(|s| s.code())
            .unwrap_or(-1);
        // Let readers capture the final bytes before taking the snapshot.
        // A descendant that escaped the group may still hold a pipe open,
        // so draining must be bounded; Drop aborts any remaining readers.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            for reader in &mut self.readers {
                let _ = reader.await;
            }
        })
        .await;
        let out_snap = self.stdout_buf.snapshot();
        let err_snap = self.stderr_buf.snapshot();
        format!(
            "[stopped, exit {code}]\n{}{}",
            String::from_utf8_lossy(&out_snap),
            String::from_utf8_lossy(&err_snap)
        )
    }

    fn signal_group(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }

    fn exited_unreaped(&mut self) -> Result<Option<i32>> {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // Inspect without reaping: retain ownership of the group ID
            // until descendants have been signalled.
            unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                if libc::waitid(
                    libc::P_PID,
                    pid,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                return Ok((info.si_pid() != 0).then(|| {
                    if info.si_code == libc::CLD_EXITED {
                        info.si_status()
                    } else {
                        -1
                    }
                }));
            }
        }
        Ok(self.child.try_wait()?.map(|s| s.code().unwrap_or(-1)))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.signal_group();
        let _ = self.child.start_kill();
        for reader in &self.readers {
            reader.abort();
        }
    }
}

async fn drain_pipe<R>(mut pipe: R, buf: OutBuf)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut tmp = [0u8; 4096];
    loop {
        match pipe.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.push(&tmp[..n]),
        }
    }
}

// Each agent owns its registry; no handles are shared between conversations.

#[derive(Default)]
pub struct ProcessManager {
    processes: HashMap<u64, Process>,
}

impl ProcessManager {
    /// Start a managed process; returns its id (monotonic, 1-based).
    pub async fn start(&mut self, command: &str, cwd: &Path) -> Result<u64> {
        static NEXT_PID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let map = &mut self.processes;
        if map.len() >= MAX_PROCS {
            let ids: Vec<String> = map
                .iter()
                .map(|(id, p)| format!("#{id}: {}", p.command))
                .collect();
            anyhow::bail!(
                "process limit reached ({MAX_PROCS}); stop one first. Running: {}",
                ids.join("; ")
            );
        }
        let id = NEXT_PID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        map.insert(id, Process::spawn(command, cwd)?);
        Ok(id)
    }

    /// Poll: status + bounded output tails (not a complete transcript).
    pub async fn poll(&mut self, id: u64) -> Result<String> {
        let map = &mut self.processes;
        Ok(map
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("no process #{id} — start one with start_process"))?
            .status()
            .await)
    }

    /// Send stdin, then report the reaction (short settle delay) in one call.
    pub async fn send_input(&mut self, id: u64, input: &str, eof: bool) -> Result<String> {
        let map = &mut self.processes;
        let p = map
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("no process #{id}"))?;
        tokio::time::timeout(std::time::Duration::from_secs(1), p.write_stdin(input, eof))
        .await.context("stdin write timed out; input may have been partially delivered; do not blindly retry")??;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        Ok(p.status().await)
    }

    /// Kill and remove a managed process.
    pub async fn stop(&mut self, id: u64) -> Result<String> {
        let map = &mut self.processes;
        let mut p = map
            .remove(&id)
            .ok_or_else(|| anyhow::anyhow!("no process #{id}"))?;
        Ok(p.kill().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stopped_ids_are_never_reused() {
        let mut pm = ProcessManager::default();
        let cwd = std::env::current_dir().unwrap();
        let first = pm.start("cat", &cwd).await.unwrap();
        pm.stop(first).await.unwrap();
        let second = pm.start("cat", &cwd).await.unwrap();
        pm.stop(second).await.unwrap();
        assert!(second > first, "stale id {first} was reused as {second}");
    }

    #[tokio::test]
    async fn start_poll_stop_lifecycle() {
        let mut pm = ProcessManager::default();
        let cwd = std::env::temp_dir();
        let id = pm.start("echo hello-process", &cwd).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let s = pm.poll(id).await.unwrap();
        assert!(s.contains("hello-process"), "{s}");
        assert!(s.contains("exited"), "{s}");
        let stop_out = pm.stop(id).await.unwrap();
        assert!(stop_out.contains("stopped"), "{stop_out}");
        assert!(pm.poll(id).await.is_err());
    }

    #[tokio::test]
    async fn interactive_stdin_and_eof() {
        let mut pm = ProcessManager::default();
        let cwd = std::env::temp_dir();
        let id = pm.start("read line; echo got:$line", &cwd).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let s = pm.send_input(id, "ping", false).await.unwrap();
        assert!(s.contains("got:ping"), "{s}");
        let s2 = pm.send_input(id, "", true).await.unwrap();
        assert!(s2.contains("exited"), "{s2}");
        pm.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn stdin_after_eof_errors() {
        let mut pm = ProcessManager::default();
        let cwd = std::env::temp_dir();
        let id = pm.start("cat", &cwd).await.unwrap();
        assert!(pm.send_input(id, "", true).await.is_ok());
        assert!(pm.send_input(id, "more", false).await.is_err());
        pm.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_id_is_a_clean_error() {
        let mut pm = ProcessManager::default();
        assert!(pm.poll(999_999).await.is_err());
        assert!(pm.stop(999_999).await.is_err());
    }

    #[tokio::test]
    async fn kill_takes_down_sleep() {
        let mut pm = ProcessManager::default();
        let cwd = std::env::temp_dir();
        let id = pm.start("sleep 30", &cwd).await.unwrap();
        let out = pm.stop(id).await.unwrap();
        assert!(out.contains("stopped"), "{out}");
    }

    #[test]
    fn output_is_bounded() {
        let buf = OutBuf::default();
        buf.push(&[b'x'; 10_000]);
        assert_eq!(buf.snapshot().len(), OUT_LIMIT);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_drains_pipes_after_group_leader_exits() {
        let cwd = std::env::current_dir().unwrap();
        let mut process = Process::spawn("sleep 30 & printf final-output", &cwd).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while process.exited_unreaped().unwrap().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shell should exit without waiting for its descendant");

        let output = process.kill().await;
        assert!(output.contains("final-output"), "{output}");
        assert!(
            process.readers.iter().all(|reader| reader.is_finished()),
            "stop must kill descendants holding pipes open and finish draining"
        );
    }
}
