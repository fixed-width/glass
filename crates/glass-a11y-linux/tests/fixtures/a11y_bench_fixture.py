#!/usr/bin/env python3
"""Large non-virtualized GTK4 tree for the glass-a11y-linux over-cap regression test.

60 Frames, each holding a vertical Box of 15 Buttons + 15 Labels -> ~60*(1+1+30) = 1920
realized accessible widgets before GTK's own scaffolding, at depth ~5 (window > box > frame >
box > widget). Non-virtualized (plain Box, not ListView) so every node is realized and walked.
The widget count alone clears MAX_NODES (1500) by a wide margin — deliberately, so the reader's
Nodes truncation bound fires on a real AT-SPI tree even if a future GTK realizes fewer accessible
nodes per widget. A short depth-20 nested Box chain exercises the depth axis independently.

Uses Gio.ApplicationFlags.NON_UNIQUE so the app skips D-Bus singleton registration and presents
its window immediately (matches a11y_fixture.py)."""
import sys
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gio", "2.0")
from gi.repository import Gio, Gtk  # noqa: E402


class BenchApp(Gtk.Application):
    def __init__(self):
        super().__init__(
            application_id="net.jesterscourt.GlassA11yBench",
            flags=Gio.ApplicationFlags.NON_UNIQUE,
        )

    def do_activate(self):
        win = Gtk.ApplicationWindow(application=self, title="Glass A11y Bench")
        win.set_default_size(400, 600)
        scroller = Gtk.ScrolledWindow()
        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)

        for f in range(60):
            frame = Gtk.Frame(label=f"Group {f:02d}")
            inner = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            for i in range(15):
                inner.append(Gtk.Button(label=f"Btn {f:02d}-{i:02d}"))
                inner.append(Gtk.Label(label=f"Lbl {f:02d}-{i:02d}"))
            frame.set_child(inner)
            outer.append(frame)

        chain = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        cursor = chain
        for d in range(20):
            nxt = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
            nxt.append(Gtk.Label(label=f"depth {d}"))
            cursor.append(nxt)
            cursor = nxt
        outer.append(chain)

        scroller.set_child(outer)
        win.set_child(scroller)
        win.present()


if __name__ == "__main__":
    BenchApp().run(sys.argv)
