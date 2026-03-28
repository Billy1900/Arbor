/// PTY and plain process runner for guest-agent.
///
/// Uses portable-pty for PTY allocation — it handles the SIGWINCH/resize
/// signalling correctly across platforms.
use anyhow::{Context, Result};
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::thread;

use arbor_common::SessionId;

// ── Session ──────────────────────────────────────────────────────────────────

pub struct Session {
    pub session_id: SessionId,
    pub pty: bool,
    master: Option<Mutex<Box<dyn MasterPty + Send>>>,
    stdin_tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    output_rx: Mutex<std::sync::mpsc::Receiver<Vec<u8>>>,
    pid: Option<u32>,
}

impl Session {
    pub fn write_stdin(&self, data: &[u8]) -> Result<()> {
        if let Some(ref tx) = self.stdin_tx {
            tx.send(data.to_vec())
                .map_err(|_| anyhow::anyhow!("stdin channel closed"))?;
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(ref master) = self.master {
            let _ = master.lock().resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    pub fn send_signal(&self, signal: i32) {
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(pid as libc::pid_t, signal);
            }
        }
    }

    /// Read available output (non-blocking, returns None if no data yet).
    pub fn try_read_output(&self) -> Option<Vec<u8>> {
        self.output_rx.lock().try_recv().ok()
    }
}

// ── Spawn ─────────────────────────────────────────────────────────────────────

pub struct PtyRunner;

impl PtyRunner {
    /// Spawn a new process, optionally with a PTY.
    /// All I/O is bridged through channels so the async caller doesn't
    /// have to worry about blocking reads.
    pub fn spawn(
        session_id: SessionId,
        command: Vec<String>,
        cwd: String,
        env: HashMap<String, String>,
        use_pty: bool,
        cols: u16,
        rows: u16,
    ) -> Result<Session> {
        if use_pty {
            Self::spawn_pty(session_id, command, cwd, env, cols, rows)
        } else {
            Self::spawn_plain(session_id, command, cwd, env)
        }
    }

    fn spawn_pty(
        session_id: SessionId,
        command: Vec<String>,
        cwd: String,
        env: HashMap<String, String>,
        cols: u16,
        rows: u16,
    ) -> Result<Session> {
        let pty_system = native_pty_system();

        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }).context("openpty failed")?;

        let mut cmd = build_command(&command, &cwd, &env);
        let child = pair.slave.spawn_command(cmd)
            .context("PTY spawn failed")?;
        let pid = child.process_id();

        // Channels
        let (stdin_tx, stdin_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);
        let (output_tx, output_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(512);

        let master = pair.master;

        // Reader thread: master → output_tx
        let output_tx2 = output_tx.clone();
        let mut reader = master.try_clone_reader().context("clone pty reader")?;
        thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if output_tx2.send(buf[..n].to_vec()).is_err() { break; }
                    }
                }
            }
        });

        // Writer thread: stdin_rx → master
        let mut writer = master.take_writer().context("take pty writer")?;
        thread::spawn(move || {
            while let Ok(data) = stdin_rx.recv() {
                if writer.write_all(&data).is_err() { break; }
            }
        });

        Ok(Session {
            session_id,
            pty: true,
            master: Some(Mutex::new(master)),
            stdin_tx: Some(stdin_tx),
            output_rx: Mutex::new(output_rx),
            pid,
        })
    }

    fn spawn_plain(
        session_id: SessionId,
        command: Vec<String>,
        cwd: String,
        env: HashMap<String, String>,
    ) -> Result<Session> {
        use std::process::{Command, Stdio};

        let program = command.first().context("empty command")?;
        let mut child = std::process::Command::new(program)
            .args(&command[1..])
            .current_dir(&cwd)
            .envs(&env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn failed")?;

        let pid = child.id();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let stdin_write = child.stdin.take().unwrap();

        let (stdin_tx, stdin_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);
        let (output_tx, output_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(512);

        // stdout reader
        let output_tx2 = output_tx.clone();
        thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => { if output_tx2.send(buf[..n].to_vec()).is_err() { break; } }
                }
            }
        });

        // stderr reader (merged into same output channel)
        thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            let mut se = stderr;
            loop {
                match se.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => { if output_tx.send(buf[..n].to_vec()).is_err() { break; } }
                }
            }
        });

        // stdin writer
        let mut sw = stdin_write;
        thread::spawn(move || {
            while let Ok(data) = stdin_rx.recv() {
                if sw.write_all(&data).is_err() { break; }
            }
        });

        Ok(Session {
            session_id,
            pty: false,
            master: None,
            stdin_tx: Some(stdin_tx),
            output_rx: Mutex::new(output_rx),
            pid: Some(pid),
        })
    }
}

fn build_command(
    command: &[String],
    cwd: &str,
    env: &HashMap<String, String>,
) -> portable_pty::CommandBuilder {
    let mut cmd = portable_pty::CommandBuilder::new(&command[0]);
    for arg in &command[1..] { cmd.arg(arg); }
    cmd.cwd(cwd);
    for (k, v) in env { cmd.env(k, v); }
    cmd
}
