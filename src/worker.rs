//! Desktop I/O in a child process the server can kill.
//!
//! Everything that talks to the desktop (the portal session, libei, screenshots, the
//! window list, wl-copy) runs in `computer-use --worker`. A call into it that does not
//! answer within its deadline gets the worker SIGKILLed and replaced on the next call.
//!
//! Why a process and not a thread: libei 1.3.x's `brei_dispatch` spins forever on a
//! zero-length message (it pops 0 bytes and loops), and a C busy loop cannot be
//! interrupted or timed out from inside the process. It held the desktop mutex at
//! 100% CPU and every later call failed with "still running" until the server was
//! restarted. Across a process boundary any hang (libei, portal, Wayland, wl-copy)
//! costs one call's deadline and nothing more.
//!
//! Protocol: one JSON object per line. The parent sends `{"id":n,"op":...}`; the worker
//! may answer `{"id":n,"ready":true}` once its input connection is up (nothing has been
//! sent to the desktop yet), then `{"id":n,"ok":<value>}` or `{"id":n,"err":"..."}`.
//! A call that hangs before `ready` emitted no input, so it is safe to retry.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::desktop::{Desktop, PasteKey};

pub const ARG: &str = "--worker";

/// Budget for bringing up the input connection: one poll of the background portal
/// negotiation (20s, desktop::APPROVAL_POLL; a consent dialog stays open across calls
/// instead), EIS connect (5s), device readiness (5s) and settling. Two attempts plus
/// the action must fit in the MCP client's 120s tool-call limit.
pub const CONNECT_BUDGET: Duration = Duration::from_secs(40);
/// How long a graceful quit may take before the worker is killed.
const QUIT_BUDGET: Duration = Duration::from_secs(3);

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Req {
    /// Liveness check; touches nothing.
    Ping,
    /// Bring up the portal session and input connection. `release` first lets go of
    /// every modifier (set after a worker died mid-action, which may have held one).
    Connect {
        #[serde(default)]
        release: bool,
    },
    Monitors,
    Screenshot { monitor: Option<String>, region: Option<(i64, i64, i64, i64)>, max_w: u32, max_h: u32 },
    Move { x: f64, y: f64 },
    Click { x: f64, y: f64, button: String, count: u32, modifiers: String, expect: Option<String> },
    Drag { from: (f64, f64), to: (f64, f64), button: String },
    Scroll { x: f64, y: f64, dx: f64, dy: f64 },
    Key { combo: String, repeat: u32, expect: Option<String> },
    Type { text: String, paste: PasteKey, expect: Option<String> },
    Clipboard { set: Option<String> },
    Windows,
    Window { query: String, action: String },
    /// Close the session and exit.
    Quit,
}

impl Req {
    /// Whether the call needs the input connection (and so a `Connect` first).
    pub fn needs_input(&self) -> bool {
        match self {
            Req::Ping
            | Req::Connect { .. }
            | Req::Window { .. }
            | Req::Quit
            | Req::Monitors
            | Req::Screenshot { .. }
            | Req::Windows
            | Req::Clipboard { .. } => false,
            Req::Move { .. } | Req::Click { .. } | Req::Drag { .. } | Req::Scroll { .. } | Req::Key { .. } | Req::Type { .. } => {
                true
            }
        }
    }

    /// Whether running it twice is harmless (reads only).
    pub fn idempotent(&self) -> bool {
        matches!(
            self,
            Req::Ping | Req::Connect { .. } | Req::Monitors | Req::Screenshot { .. } | Req::Windows | Req::Clipboard { set: None }
        )
    }

    /// Deadline for one call, once the worker is connected.
    pub fn budget(&self) -> Duration {
        let s = Duration::from_secs;
        let ms = Duration::from_millis;
        match self {
            Req::Ping | Req::Quit => s(5),
            Req::Connect { .. } => CONNECT_BUDGET,
            // Direct capture (8s per output), then the portal fallback (20s), then scaling.
            Req::Screenshot { .. } => s(45),
            Req::Key { repeat, .. } => s(10) + ms(60) * (*repeat).min(10_000),
            Req::Click { count, .. } => s(10) + ms(100) * (*count).min(1_000),
            // keystrokes run ~10ms each; a paste adds wl-copy and the focus check.
            Req::Type { text, .. } => s(15) + ms(25) * text.chars().count().min(100_000) as u32,
            _ => s(15),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct Envelope {
    id: u64,
    #[serde(flatten)]
    req: Req,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct Reply {
    id: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ok: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    err: Option<String>,
}

// ---- worker side ----------------------------------------------------------------------

unsafe extern "C" {
    fn getppid() -> i32;
    fn _exit(code: i32) -> !;
}

/// Child entry point: serve requests from stdin until it closes or the parent dies.
pub fn run() -> Result<()> {
    // If the server dies (even by SIGKILL) while this process is stuck in a C loop,
    // stdin EOF is never read; watch for reparenting instead.
    let parent = unsafe { getppid() };
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if unsafe { getppid() } != parent {
                unsafe { _exit(0) };
            }
        }
    });
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
        // portal.rs drives its async calls with Handle::block_on, so the loop runs on a
        // blocking thread that has the runtime context.
        tokio::task::spawn_blocking(serve).await?
    })
}

fn serve() -> Result<()> {
    let mut desk = Desktop::new();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut send = |reply: &Reply| -> Result<()> {
        writeln!(out, "{}", serde_json::to_string(reply)?)?;
        out.flush()?;
        Ok(())
    };
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let env: Envelope = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                send(&Reply { id: 0, err: Some(format!("bad request: {e}")), ..Default::default() })?;
                continue;
            }
        };
        let id = env.id;
        if matches!(env.req, Req::Quit) {
            send(&Reply { id, ok: Some(Value::Null), ..Default::default() })?;
            return Ok(());
        }
        let result = (|| -> Result<Value> {
            if env.req.needs_input() {
                desk.connect(false)?;
                check_focus(&mut desk, &env.req)?;
                send(&Reply { id, ready: true, ..Default::default() })?;
            }
            execute(&mut desk, env.req)
        })();
        let reply = match result {
            Ok(v) => Reply { id, ok: Some(v), ..Default::default() },
            Err(e) => Reply { id, err: Some(format!("{e:#}")), ..Default::default() },
        };
        send(&reply)?;
    }
    Ok(())
}

/// Refuse input when the caller named the window it expects to have focus and a
/// different one has it: typing into the wrong window is the costliest mistake here.
fn check_focus(desk: &mut Desktop, req: &Req) -> Result<()> {
    let expect = match req {
        Req::Key { expect, .. } | Req::Type { expect, .. } | Req::Click { expect, .. } => expect.as_deref(),
        _ => None,
    };
    let Some(expect) = expect.map(str::trim).filter(|e| !e.is_empty()) else { return Ok(()) };
    let focused = desk.focused();
    let ok = focused.as_ref().is_some_and(|w| {
        let q = expect.to_lowercase();
        w.id == expect || w.title.to_lowercase().contains(&q) || w.app_id.to_lowercase().contains(&q)
    });
    if !ok {
        let now = focused.map_or("no window".to_owned(), |w| format!("{:?} ({})", w.title, w.app_id));
        bail!(
            "nothing was sent: the focused window is {now}, not one matching {expect:?}. Focus the right window (`window` tool), take a screenshot, then retry"
        );
    }
    Ok(())
}

/// What an input action reports: where the keyboard focus is afterwards.
fn focus_report(desk: &mut Desktop) -> Value {
    match desk.focused() {
        Some(w) => json!({ "focused": { "id": w.id, "title": w.title, "app_id": w.app_id } }),
        None => json!({ "focused": null }),
    }
}

fn execute(desk: &mut Desktop, req: Req) -> Result<Value> {
    Ok(match req {
        Req::Ping => json!("pong"),
        Req::Connect { release } => {
            desk.connect(release)?;
            Value::Null
        }
        Req::Monitors => serde_json::to_value(desk.monitors()?)?,
        Req::Screenshot { monitor, region, max_w, max_h } => {
            let (png, shot) = desk.screenshot(monitor.as_deref(), region, max_w, max_h)?;
            json!({ "png": base64::engine::general_purpose::STANDARD.encode(png), "shot": shot })
        }
        Req::Move { x, y } => {
            desk.move_to(x, y)?;
            Value::Null
        }
        Req::Click { x, y, button, count, modifiers, .. } => {
            desk.click(x, y, &button, count, &modifiers)?;
            sleep_settle();
            focus_report(desk)
        }
        Req::Drag { from, to, button } => {
            desk.drag(from, to, &button)?;
            Value::Null
        }
        Req::Scroll { x, y, dx, dy } => {
            desk.scroll(x, y, dx, dy)?;
            Value::Null
        }
        Req::Key { combo, repeat, .. } => {
            desk.key(&combo, repeat)?;
            sleep_settle();
            focus_report(desk)
        }
        Req::Type { text, paste, .. } => {
            let done = desk.type_text(&text, paste)?;
            let mut v = focus_report(desk);
            v["done"] = json!(done);
            v
        }
        Req::Clipboard { set } => json!(desk.clipboard(set.as_deref())?),
        Req::Windows => serde_json::to_value(desk.windows()?)?,
        Req::Window { query, action } => serde_json::to_value(desk.window_action(&query, &action)?)?,
        Req::Quit => Value::Null,
    })
}

/// Let a click or shortcut take effect before reading back the focus.
fn sleep_settle() {
    std::thread::sleep(Duration::from_millis(60));
}

// ---- server side ----------------------------------------------------------------------

/// Builds the command that starts a worker (the real binary, or a stand-in in tests).
pub type Spawner = Box<dyn Fn() -> Command + Send>;

struct Proc {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    connected: bool,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Server-side handle: at most one live worker, replaced whenever a call hangs.
pub struct Backend {
    spawn: Spawner,
    proc: Option<Proc>,
    next_id: u64,
    /// Upper bound on every deadline (tests use it to fail fast).
    cap: Option<Duration>,
    /// A worker died after it may have pressed something: the next connection first
    /// releases every modifier so nothing stays held on the compositor side.
    release_next: bool,
}

/// What became of a call that did not complete.
#[derive(Debug)]
enum Failure {
    /// Worker answered with an error: report it as is.
    Answered(String),
    /// Worker hung or died; `emitted` = it may already have sent input to the desktop.
    Lost { emitted: bool, why: String },
}

impl Backend {
    pub fn new() -> Self {
        Self::with_spawner(Box::new(|| {
            let mut cmd = Command::new(crate::self_exe());
            cmd.arg(ARG);
            cmd
        }))
    }

    pub fn with_spawner(spawn: Spawner) -> Self {
        Self { spawn, proc: None, next_id: 1, cap: None, release_next: false }
    }

    /// Whether a worker is running (and so may hold a portal session).
    pub fn is_running(&mut self) -> bool {
        self.live().is_some()
    }

    /// Run one request with its deadline. A hung or dead worker is killed; a call that
    /// provably sent no input (or only reads) is retried once on a fresh worker.
    pub fn call(&mut self, req: Req) -> Result<Value> {
        match self.attempt(&req) {
            Ok(v) => Ok(v),
            Err(Failure::Answered(e)) => Err(anyhow!(e)),
            Err(Failure::Lost { emitted, why }) if !emitted || req.idempotent() => {
                eprintln!("computer-use: worker lost ({why}); retrying on a fresh worker");
                match self.attempt(&req) {
                    Ok(v) => Ok(v),
                    Err(Failure::Answered(e)) => Err(anyhow!(e)),
                    Err(Failure::Lost { why: again, .. }) => Err(anyhow!(
                        "the desktop backend hung twice ({why}; then {again}); it was restarted each time. \
                         Try again; if this keeps happening the portal or compositor needs attention"
                    )),
                }
            }
            Err(Failure::Lost { why, .. }) => Err(anyhow!(
                "the desktop backend hung mid-action ({why}) and was restarted; part of the action may \
                 have happened. Take a screenshot to check before retrying"
            )),
        }
    }

    /// Stop the worker: ask it to close its session, then make sure it is gone.
    pub fn stop(&mut self) {
        if let Some(mut p) = self.proc.take() {
            let id = self.next_id;
            self.next_id += 1;
            if Self::send(&mut p, id, &Req::Quit).is_ok() {
                let _ = Self::wait(&mut p, id, QUIT_BUDGET);
            }
            // Drop kills it if it is still there.
        }
    }

    fn attempt(&mut self, req: &Req) -> Result<Value, Failure> {
        let lost = |emitted: bool, why: String| Failure::Lost { emitted, why };
        if req.needs_input() && !self.live().is_some_and(|p| p.connected) {
            let connect = Req::Connect { release: self.release_next };
            self.exchange(&connect, self.budget(&connect)).map_err(|f| match f {
                Failure::Lost { why, .. } => lost(false, why),
                other => other,
            })?;
            self.release_next = false;
            if let Some(p) = self.proc.as_mut() {
                p.connected = true;
            }
        }
        let r = self.exchange(req, self.budget(req));
        if matches!(r, Err(Failure::Lost { emitted: true, .. })) {
            self.release_next = true;
        }
        r
    }

    fn budget(&self, req: &Req) -> Duration {
        let b = req.budget();
        self.cap.map_or(b, |c| b.min(c))
    }

    /// One request/reply on the live worker (spawned if needed). Never blocks past `budget`.
    fn exchange(&mut self, req: &Req, budget: Duration) -> Result<Value, Failure> {
        let id = self.next_id;
        self.next_id += 1;
        let r = match self.ensure() {
            Err(e) => Err(Failure::Lost { emitted: false, why: format!("{e:#}") }),
            Ok(p) => match Self::send(p, id, req) {
                Err(e) => Err(Failure::Lost { emitted: false, why: format!("could not reach the worker: {e:#}") }),
                Ok(()) => Self::wait(p, id, budget),
            },
        };
        if matches!(r, Err(Failure::Lost { .. })) {
            self.proc = None; // dropping kills it
        }
        r
    }

    fn live(&mut self) -> Option<&mut Proc> {
        if let Some(p) = self.proc.as_mut() {
            if matches!(p.child.try_wait(), Ok(None)) {
                return self.proc.as_mut();
            }
        }
        self.proc = None;
        None
    }

    fn ensure(&mut self) -> Result<&mut Proc> {
        if self.live().is_none() {
            let mut cmd = (self.spawn)();
            let mut child = cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::piped()) // never inherit: our stdout is the MCP stream
                .stderr(Stdio::inherit())
                .spawn()
                .context("spawn desktop worker")?;
            let stdin = child.stdin.take().context("worker stdin")?;
            let stdout = child.stdout.take().context("worker stdout")?;
            let (tx, lines) = mpsc::channel();
            // Blocking pipe reads stay off the calling thread so waits can time out.
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
            self.proc = Some(Proc { child, stdin, lines, connected: false });
        }
        Ok(self.proc.as_mut().unwrap())
    }

    fn send(p: &mut Proc, id: u64, req: &Req) -> Result<()> {
        let line = serde_json::to_string(&Envelope { id, req: req.clone() })?;
        writeln!(p.stdin, "{line}")?;
        p.stdin.flush()?;
        Ok(())
    }

    fn wait(p: &mut Proc, id: u64, budget: Duration) -> Result<Value, Failure> {
        let deadline = Instant::now() + budget;
        let mut emitted = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match p.lines.recv_timeout(left) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Failure::Lost {
                        emitted,
                        why: format!("no answer within {}s", budget.as_secs()),
                    });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Failure::Lost { emitted, why: "worker exited".into() });
                }
            };
            let Ok(reply) = serde_json::from_str::<Reply>(&line) else { continue };
            if reply.id != id {
                continue; // a late answer to a call we gave up on
            }
            if reply.ready {
                emitted = true;
                continue;
            }
            return match (reply.ok, reply.err) {
                (_, Some(e)) => Err(Failure::Answered(e)),
                (Some(v), None) => Ok(v),
                (None, None) => Ok(Value::Null),
            };
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Decode a screenshot reply into PNG bytes and its coordinate mapping.
pub fn screenshot_reply(v: Value) -> Result<(Vec<u8>, crate::desktop::Shot)> {
    let png = v.get("png").and_then(Value::as_str).ok_or_else(|| anyhow!("screenshot reply without png"))?;
    let png = base64::engine::general_purpose::STANDARD.decode(png)?;
    let shot = serde_json::from_value(v.get("shot").cloned().unwrap_or(Value::Null))?;
    if png.is_empty() {
        bail!("empty screenshot");
    }
    Ok((png, shot))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &'static str) -> Backend {
        let mut b = Backend::with_spawner(Box::new(move || {
            let mut c = Command::new("sh");
            c.arg("-c").arg(script);
            c
        }));
        b.cap = Some(Duration::from_millis(700));
        b
    }

    /// A stand-in worker that answers every request with ok, echoing its id.
    const ECHO: &str = r#"while IFS= read -r l; do id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); printf '{"id":%s,"ok":"done"}\n' "$id"; done"#;

    #[test]
    fn answers_come_back() {
        let mut b = sh(ECHO);
        assert_eq!(b.call(Req::Ping).unwrap(), json!("done"));
        assert_eq!(b.call(Req::Key { combo: "a".into(), repeat: 1, expect: None }).unwrap(), json!("done"));
    }

    #[test]
    fn a_hung_worker_is_killed_and_the_call_returns() {
        // Never answers anything: Connect hangs, the worker is killed, the retry hangs too.
        let mut b = sh("exec sleep 1000");
        let started = Instant::now();
        let err = b.call(Req::Ping).unwrap_err();
        assert!(format!("{err:#}").contains("hung twice"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        assert!(b.proc.is_none(), "hung worker must be gone");
    }

    #[test]
    fn a_worker_that_hangs_once_is_replaced_and_the_call_retried() {
        // First worker hangs; the replacement (marker file exists) echoes.
        let dir = std::env::temp_dir().join(format!("cu-worker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("seen");
        let script = format!(
            "if [ -e '{m}' ]; then {echo}; else touch '{m}'; exec sleep 1000; fi",
            m = marker.display(),
            echo = ECHO
        );
        let script: &'static str = Box::leak(script.into_boxed_str());
        let mut b = sh(script);
        assert_eq!(b.call(Req::Ping).unwrap(), json!("done"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn input_that_may_have_been_sent_is_not_replayed() {
        // Answers Connect, says "ready" for the key, then hangs: the key must not be retried.
        let script = r#"while IFS= read -r l; do id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); case "$l" in *'"op":"connect"'*) printf '{"id":%s,"ok":null}\n' "$id";; *) printf '{"id":%s,"ready":true}\n' "$id"; exec sleep 1000;; esac; done"#;
        let mut b = sh(script);
        let started = Instant::now();
        let err = b.call(Req::Move { x: 1.0, y: 1.0 }).unwrap_err();
        assert!(format!("{err:#}").contains("mid-action"), "{err:#}");
        // One capped budget, no retry.
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
    }

    #[test]
    fn after_an_interrupted_action_the_next_connect_releases_modifiers() {
        // Logs every request; hangs on the first key (after "ready"), answers the rest.
        let dir = std::env::temp_dir().join(format!("cu-release-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let marker = dir.join("hung");
        let script = format!(
            r#"while IFS= read -r l; do printf '%s\n' "$l" >> '{log}'; id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); case "$l" in *'"op":"key"'*) if [ ! -e '{m}' ]; then touch '{m}'; printf '{{"id":%s,"ready":true}}\n' "$id"; exec sleep 1000; fi;; esac; printf '{{"id":%s,"ok":null}}\n' "$id"; done"#,
            log = log.display(),
            m = marker.display()
        );
        let script: &'static str = Box::leak(script.into_boxed_str());
        let mut b = sh(script);
        let key = || Req::Key { combo: "ctrl+a".into(), repeat: 1, expect: None };
        assert!(format!("{:#}", b.call(key()).unwrap_err()).contains("mid-action"));
        b.call(key()).unwrap();
        let lines = std::fs::read_to_string(&log).unwrap();
        let connects: Vec<&str> = lines.lines().filter(|l| l.contains(r#""op":"connect""#)).collect();
        assert_eq!(connects.len(), 2, "{lines}");
        assert!(connects[0].contains(r#""release":false"#), "{lines}");
        assert!(connects[1].contains(r#""release":true"#), "{lines}");
        drop(b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn worker_errors_are_reported_not_retried() {
        let mut b = sh(r#"while IFS= read -r l; do id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); printf '{"id":%s,"err":"nope"}\n' "$id"; done"#);
        let err = b.call(Req::Ping).unwrap_err();
        assert_eq!(format!("{err:#}"), "nope");
    }

    #[test]
    fn requests_round_trip_as_json() {
        let r = Req::Screenshot { monitor: None, region: Some((1, 2, 3, 4)), max_w: 10, max_h: 20 };
        let s = serde_json::to_string(&Envelope { id: 7, req: r }).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, 7);
        assert!(matches!(back.req, Req::Screenshot { region: Some((1, 2, 3, 4)), .. }));
    }
}
