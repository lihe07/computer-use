# computer-use

Computer-use MCP server for COSMIC/Wayland. Input goes through the xdg-desktop-portal
RemoteDesktop session and libei; screenshots, the monitor layout, the window list and
window actions talk to the compositor directly (ext-image-copy-capture, xdg-output,
COSMIC toplevel protocols) and never need the portal.

Call `control` with `enabled=true` before using any input tool, then
`enabled=false` when finished or waiting for the user. The owner displays an
"Agent is in control" banner. `screenshot`, `windows` and `monitors` only read and
work without control.

Control is exclusive across all server processes for the same logged-in user.
An OS file lock at `/run/user/<uid>/computer-use.lock` stays held for the whole
control session, including while the banner is temporarily hidden for screenshots
or if the banner fails. A competing agent receives a busy tool error naming the
holder (server pid, Claude session name, working directory, how long) and must
wait before retrying. It cannot use any input tool or release the owner's lock.
Repeated acquisition by the owner keeps the same lock. Release clears input
ownership; the last screenshot's coordinate mapping is kept for the next control
session, and input results warn when it is more than 30 seconds old.

An owner that makes no desktop call for 5 minutes loses control automatically and
is told so on its next call. The OS releases ownership when the server exits or
crashes. The lock file stays in place; its existence does not mean control is
held. Do not delete it to reset ownership, since that can allow two processes to
lock different files.

All agents must run a version with this locking support. Older server processes
do not honor the lock and must be restarted after upgrading.

## Input safety

- `key`, `type_text` and `click` take `expect_window` (a title or app_id
  substring). If a different window has keyboard focus, nothing is sent. Their
  results always report the focused window afterwards.
- When a call fails, input calls that were sent in the same batch (queued behind
  it, or arriving within 1.5s of the failure) are skipped instead of acting on a
  plan that no longer holds. Reads and `control` are never skipped.
- A key or button pressed by an action is always released, even if the action
  fails halfway. If a worker is killed mid-action, the next connection first
  releases every modifier.
- `type_text` types at most 3000 ASCII characters per call so a call never
  outlives the client's 120s tool-call limit; non-ASCII text is pasted via the
  clipboard, which then holds that text.
- If the HUD cannot be shown, control is still granted but every result carries a
  warning, and the HUD is retried every 30 seconds.

## Permission dialog

The first input after login (or after the saved restore token stops working)
asks the user to allow remote control. The request runs in the background and its
dialog stays open for 5 minutes; an input call waits up to 20 seconds for it and
then returns "waiting for the user to allow remote control" instead of blocking.
The next call picks up the approval. Each call no longer opens and abandons its
own dialog. After control is released the session is kept for 10 minutes, so the
next `control` does not need a new approval.

## Hangs

All desktop I/O (the portal session, libei input, screenshots, the window list,
wl-copy) runs in a child process, `computer-use --worker`. The server never calls
into the desktop itself; it sends each request to the worker with a deadline.
A worker that misses a deadline, or dies, is SIGKILLed and replaced on the next
call, which reconnects with the saved restore token (no dialog). So a hang costs
one call's deadline, never the session.

A call that hung before it sent any input, or that only reads (screenshot,
monitors, windows, reading the clipboard), is retried once on a fresh worker
automatically. A call that
may already have sent input reports an error instead, so a click or keystroke is
never replayed; take a screenshot before retrying it.

The worker exists because libei 1.3.x spins forever in `brei_dispatch` on a
zero-length message (it pops 0 bytes and loops). That busy loop cannot be
interrupted from inside the process: it used to hold the desktop at 100% CPU
until the server was restarted.

Build with Rust 1.89 or newer using `cargo build --release --locked`.
Run `cargo test --locked` for ownership and tool-gating checks without touching
the live desktop.

The worker and HUD are started from `/proc/self/exe`, so reinstalling the binary
while servers run no longer breaks them with "No such file or directory".

## Portal timeouts on COSMIC

Screenshots only fall back to the screenshot portal when direct capture fails. If
input (or that fallback) times out even after restarting the MCP, inspect
`systemctl --user status xdg-desktop-portal.service`. A user service manager
restarted from a terminal or SSH session can lose `XDG_CURRENT_DESKTOP` and
`WAYLAND_DISPLAY`. The portal then selects the wrong backend or cannot connect
to the compositor. Changing the MCP's own environment does not repair the
environment used by D-Bus and systemd to activate portal services.

Release desktop control, then run `computer-use --repair-portal`. This explicit
command restores only graphical session variables from a running, same-user
`cosmic-comp` and its owned listening socket, updates the D-Bus/systemd activation environment, and restarts the
COSMIC backend and portal frontend. **Existing screen-sharing sessions will be
interrupted.** It does not restart the compositor or log you out. Each external
command has a deadline; errors are reported without an automatic retry.

Repair refuses to run while another agent owns desktop control or if the COSMIC
session is ambiguous. For multiple sessions, select the intended one with
`WAYLAND_DISPLAY=wayland-1 computer-use --repair-portal` (use your actual display).
Then retry `control` and `screenshot`. Permission denial and cancelled capture
requests are still returned normally; this does not bypass portal permissions.

When the MCP host omits `WAYLAND_DISPLAY`, or sets one that is not a live socket,
startup also uses the compositor-owned socket. It no longer picks the first `wayland-*` entry, which may belong to an
outer compositor when COSMIC is nested. Explicit display settings are preserved.
