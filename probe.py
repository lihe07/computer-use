"""Open a portal session, connect to EIS, list devices; optionally click at logical X Y."""
import sys
import time

from cu import ei
from cu.portal import Portal, RemoteSession

s = RemoteSession(Portal()).start()
print("streams", s.streams)
c = ei.EIClient(s.connect_to_eis())
c.wait_ready([ei.CAP_POINTER_ABSOLUTE, ei.CAP_BUTTON, ei.CAP_KEYBOARD])
for d in c.devices.values():
    print(d, "resumed" if d.resumed else "paused")

if len(sys.argv) >= 3:
    x, y = float(sys.argv[1]), float(sys.argv[2])
    c.move_abs(x, y)
    time.sleep(0.05)
    if "--click" in sys.argv:
        c.button(0x110, True)
        time.sleep(0.03)
        c.button(0x110, False)
    print("moved", x, y)
    for _ in range(20):
        c.pump(0.05)
c.close()
s.close()
