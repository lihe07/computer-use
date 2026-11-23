# computer-use

Computer-use MCP server for XFCE/X11 and COSMIC/Wayland. On X11 it uses
GetImage for screenshots, XTEST for input, EWMH for window management, and
RandR 1.5 for monitor layout. COSMIC uses xdg-desktop-portal
RemoteDesktop and libei.

## Running on XFCE/X11

Run the MCP server from the graphical session as the desktop user. It selects
X11 when `XDG_SESSION_TYPE=x11` or when `DISPLAY` is set and
`WAYLAND_DISPLAY` is absent. The X11 path needs access to that X server and
does not open a portal permission dialog. If the MCP host strips session
variables, set `DISPLAY` and, if required, `XAUTHORITY` in its server environment.
For example, on the first TigerVNC desktop run `DISPLAY=:1 XDG_SESSION_TYPE=x11
/path/to/computer-use`. Install `xclip` for non-ASCII clipboard paste on X11.

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

Desktop requests run in a child process, `computer-use --worker`. The server
sends each request to the worker with a deadline. A worker that misses a deadline
or dies is replaced. The new worker opens a fresh X11 connection or reconnects
to the Wayland portal with the saved restore token.

A call that hung before it sent any input, or that only reads (screenshot,
monitors, windows), is retried once on a fresh worker automatically. A call that
may already have sent input reports an error instead, so a click or keystroke is
never replayed; take a screenshot before retrying it.

The worker exists because libei 1.3.x spins forever in `brei_dispatch` on a
zero-length message (it pops 0 bytes and loops). That busy loop cannot be
interrupted from inside the process: it used to hold the desktop at 100% CPU
until the server was restarted.

Build with Rust 1.89 or newer using `cargo build --release --locked`.
Both backends are included in the binary, which links to libei,
libwayland-client and libxkbcommon. Point the MCP host's stdio command at `target/release/computer-use`
or copy that binary to the location configured by your host.
Run `cargo test --locked` for ownership and tool-gating checks without touching
the live desktop.

## Screenshot portal timeouts on COSMIC

If screenshots time out even after restarting the MCP, inspect
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

When the MCP host omits `WAYLAND_DISPLAY`, startup also uses the compositor-owned
socket. It no longer picks the first `wayland-*` entry, which may belong to an
outer compositor when COSMIC is nested. Explicit display settings are preserved.
