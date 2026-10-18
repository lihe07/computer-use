"""xdg-desktop-portal RemoteDesktop session that hands back an EIS socket."""
import os
import uuid

from gi.repository import Gio, GLib

DEST = "org.freedesktop.portal.Desktop"
PATH = "/org/freedesktop/portal/desktop"
RD = "org.freedesktop.portal.RemoteDesktop"
SC = "org.freedesktop.portal.ScreenCast"
SS = "org.freedesktop.portal.Screenshot"
STATE_DIR = os.path.expanduser("~/.local/state/computer-use")
TOKEN_FILE = os.path.join(STATE_DIR, "restore_token")


class PortalError(RuntimeError):
    pass


def _ensure_session_env():
    """MCP hosts may launch us with a scrubbed env; recover the user session's bus."""
    runtime = os.environ.setdefault("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    os.environ.setdefault("DBUS_SESSION_BUS_ADDRESS", f"unix:path={runtime}/bus")


class Portal:
    def __init__(self):
        _ensure_session_env()
        self.bus = Gio.bus_get_sync(Gio.BusType.SESSION)
        self.sender = self.bus.get_unique_name()[1:].replace(".", "_")

    def request(self, iface, method, args_fmt, args, opts, timeout=120):
        """Call a portal method returning a Request; block until its Response."""
        token = "cu" + uuid.uuid4().hex[:12]
        opts = dict(opts, handle_token=GLib.Variant("s", token))
        req_path = f"/org/freedesktop/portal/desktop/request/{self.sender}/{token}"
        ctx = GLib.MainContext.new()
        ctx.push_thread_default()
        try:
            loop = GLib.MainLoop.new(ctx, False)
            result = {}

            def on_response(_c, _s, _p, _i, _sig, params):
                result["code"], result["res"] = params.unpack()
                loop.quit()

            def on_timeout():
                loop.quit()
                return False

            sub = self.bus.signal_subscribe(DEST, "org.freedesktop.portal.Request", "Response",
                                            req_path, None, Gio.DBusSignalFlags.NO_MATCH_RULE,
                                            on_response)
            self.bus.call_sync(DEST, PATH, iface, method,
                               GLib.Variant(f"({args_fmt}a{{sv}})", (*args, opts)),
                               None, Gio.DBusCallFlags.NONE, -1, None)
            src = GLib.timeout_source_new_seconds(timeout)
            src.set_callback(on_timeout)
            src.attach(ctx)
            loop.run()
            src.destroy()
            self.bus.signal_unsubscribe(sub)
        finally:
            ctx.pop_thread_default()
        if "code" not in result:
            raise PortalError(f"{method}: timed out waiting for portal response")
        if result["code"] != 0:
            reason = {1: "cancelled by user", 2: "failed"}.get(result["code"], result["code"])
            raise PortalError(f"{method}: {reason}")
        return result["res"]

    def screenshot(self):
        """Non-interactive full-desktop screenshot; returns a local file path."""
        res = self.request(SS, "Screenshot", "s", ("",), {"interactive": GLib.Variant("b", False)})
        uri = res["uri"]
        return GLib.filename_from_uri(uri)[0]


class RemoteSession:
    """A RemoteDesktop session with keyboard+pointer and all monitors attached.

    The restore token is persisted, so the approval dialog only shows once.
    """

    def __init__(self, portal: Portal):
        self.portal = portal
        self.handle = None
        self.streams = []

    def start(self):
        p = self.portal
        res = p.request(RD, "CreateSession", "", (),
                        {"session_handle_token": GLib.Variant("s", "cu" + uuid.uuid4().hex[:8])})
        self.handle = res["session_handle"]
        dev_opts = {"types": GLib.Variant("u", 3), "persist_mode": GLib.Variant("u", 2)}
        if os.path.exists(TOKEN_FILE):
            dev_opts["restore_token"] = GLib.Variant("s", open(TOKEN_FILE).read().strip())
        p.request(RD, "SelectDevices", "o", (self.handle,), dev_opts)
        p.request(SC, "SelectSources", "o", (self.handle,),
                  {"types": GLib.Variant("u", 1), "multiple": GLib.Variant("b", True)})
        res = p.request(RD, "Start", "os", (self.handle, ""), {})
        if "restore_token" in res:
            os.makedirs(STATE_DIR, exist_ok=True)
            with open(TOKEN_FILE, "w") as f:
                f.write(res["restore_token"])
        self.streams = res.get("streams", [])
        return self

    def connect_to_eis(self) -> int:
        """Return a new fd connected to the compositor's EIS server."""
        reply, fds = self.portal.bus.call_with_unix_fd_list_sync(
            DEST, PATH, RD, "ConnectToEIS",
            GLib.Variant("(oa{sv})", (self.handle, {})),
            GLib.VariantType("(h)"), Gio.DBusCallFlags.NONE, -1, None, None)
        return fds.get(reply.unpack()[0])

    def close(self):
        if self.handle:
            try:
                self.portal.bus.call_sync(DEST, self.handle, "org.freedesktop.portal.Session",
                                          "Close", None, None, Gio.DBusCallFlags.NONE, -1, None)
            except GLib.Error:
                pass
            self.handle = None
