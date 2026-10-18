"""MCP server exposing desktop control for COSMIC/Wayland."""
import asyncio
from typing import Literal

from mcp.server.mcpserver import Image, MCPServer

from .desktop import Desktop

INSTRUCTIONS = """\
Controls the user's real Wayland desktop (moves their actual cursor).
Coordinates: by default x/y are pixels in the LAST screenshot image returned by this server
(space="image"); the server maps them to the desktop, including crops and downscaling.
Take a fresh screenshot before acting, and again after to verify. To click small targets
precisely, take a screenshot with `region` around the area (it is upscaled) and click in that.
Pass space="desktop" to use logical desktop coordinates directly (see `monitors`).
"""

mcp = MCPServer("computer-use", instructions=INSTRUCTIONS)
desk = Desktop()
Space = Literal["image", "desktop"]
Button = Literal["left", "right", "middle", "back", "forward"]


async def run(fn, *args, **kw):
    return await asyncio.to_thread(fn, *args, **kw)


@mcp.tool()
async def monitors() -> str:
    """List monitors as logical desktop rectangles (left to right)."""
    mons = await run(desk.monitors)
    return "\n".join(f"{i}: {n}  x={x} y={y} {w}x{h} scale={s}"
                     for i, (n, x, y, w, h, s) in enumerate(mons))


@mcp.tool()
async def screenshot(monitor: str | None = None,
                     region: list[int] | None = None,
                     max_width: int = 1920,
                     max_height: int = 1200) -> list:
    """Capture the desktop. Subsequent x/y (space="image") refer to pixels of this image.

    monitor: name (e.g. "HDMI-A-1") or index from `monitors` to capture one screen.
    region: [x, y, w, h] in logical desktop coords to zoom into (upscaled up to 4x).
    """
    data, shot = await run(desk.screenshot, monitor, tuple(region) if region else None,
                           max_width, max_height)
    note = (f"image {shot.width}x{shot.height}; desktop = ({shot.origin_x}, {shot.origin_y}) "
            f"+ image_px / {shot.scale:g}")
    return [Image(data=data, format="png"), note]


@mcp.tool()
async def click(x: float, y: float, button: Button = "left", count: int = 1,
                modifiers: str = "", space: Space = "image") -> str:
    """Click at (x, y). count=2 for double-click. modifiers like "ctrl" or "ctrl+shift"."""
    dx, dy = desk.resolve(x, y, space)
    await run(desk.click, dx, dy, button, count, modifiers)
    return f"clicked {button} x{count} at desktop ({dx:.0f}, {dy:.0f})"


@mcp.tool()
async def move(x: float, y: float, space: Space = "image") -> str:
    """Move the pointer without clicking (hover)."""
    dx, dy = desk.resolve(x, y, space)
    await run(desk.move, dx, dy)
    return f"moved to desktop ({dx:.0f}, {dy:.0f})"


@mcp.tool()
async def drag(x1: float, y1: float, x2: float, y2: float, button: Button = "left",
               space: Space = "image") -> str:
    """Press at (x1, y1), move to (x2, y2), release."""
    a = desk.resolve(x1, y1, space)
    b = desk.resolve(x2, y2, space)
    await run(desk.drag, *a, *b, button)
    return f"dragged desktop ({a[0]:.0f}, {a[1]:.0f}) -> ({b[0]:.0f}, {b[1]:.0f})"


@mcp.tool()
async def scroll(x: float, y: float, dy: float = 3, dx: float = 0,
                 space: Space = "image") -> str:
    """Scroll at (x, y). dy/dx in wheel notches; positive dy scrolls down."""
    px, py = desk.resolve(x, y, space)
    await run(desk.scroll, px, py, dx, dy)
    return f"scrolled ({dx}, {dy}) at desktop ({px:.0f}, {py:.0f})"


@mcp.tool()
async def key(combo: str, repeat: int = 1) -> str:
    """Press a key or chord, e.g. "enter", "ctrl+c", "super", "alt+tab", "ctrl+shift+t"."""
    await run(desk.key, combo, repeat)
    return f"pressed {combo}" + (f" x{repeat}" if repeat > 1 else "")


@mcp.tool()
async def type_text(text: str) -> str:
    """Type text into the focused window (US layout). Non-ASCII is pasted via clipboard."""
    return await run(desk.type_text, text)


def main():
    mcp.run("stdio")


if __name__ == "__main__":
    main()
