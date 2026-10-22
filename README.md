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

Build with Rust 1.89 or newer using `cargo build --release --locked`.
Run `cargo test --locked` for ownership and tool-gating checks without touching
the live desktop.
