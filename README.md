# computer-use

Computer-use MCP server for COSMIC/Wayland via xdg-desktop-portal RemoteDesktop and libei.

Call `control` with `enabled=true` before using any desktop tool, then
`enabled=false` when finished or waiting for the user. The owner displays an
"Agent is in control" banner.

Control is exclusive across all server processes for the same logged-in user.
An OS file lock at `/run/user/<uid>/computer-use.lock` stays held for the whole
control session, including while the banner is temporarily hidden for screenshots
or if the banner fails. A competing agent receives a busy tool error and must
wait before retrying. It cannot use any desktop tool or release the owner's lock.
Repeated acquisition by the owner keeps the same lock. Release clears the old
screenshot coordinates and disconnects input before allowing another owner.

The OS releases ownership when the server exits or crashes. The lock file stays
in place; its existence does not mean control is held. Do not delete it to reset
ownership, since that can allow two processes to lock different files.

All agents must run a version with this locking support. Older server processes
do not honor the lock and must be restarted after upgrading.

## Hangs

All desktop I/O (the portal session, libei input, screenshots, the window list,
wl-copy) runs in a child process, `computer-use --worker`. The server never calls
into the desktop itself; it sends each request to the worker with a deadline.
A worker that misses a deadline, or dies, is SIGKILLed and replaced on the next
call, which reconnects with the saved restore token (no dialog). So a hang costs
one call's deadline, never the session.

A call that hung before it sent any input, or that only reads (screenshot,
monitors, windows), is retried once on a fresh worker automatically. A call that
may already have sent input reports an error instead, so a click or keystroke is
never replayed; take a screenshot before retrying it.

The worker exists because libei 1.3.x spins forever in `brei_dispatch` on a
zero-length message (it pops 0 bytes and loops). That busy loop cannot be
interrupted from inside the process: it used to hold the desktop at 100% CPU
until the server was restarted.

Build with Rust 1.89 or newer using `cargo build --release --locked`.
Run `cargo test --locked` for ownership and tool-gating checks without touching
the live desktop.
