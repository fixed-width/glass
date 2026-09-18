#!/usr/bin/env python3
"""Separate GTK3 processes with independent counters and a controllable input region."""
import os
from pathlib import Path
import sys

import cairo
import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk

folder = Path(sys.argv[1])
cover_mode = len(sys.argv) > 2
width = int(sys.argv[2]) if cover_mode else 400
name = "cover" if cover_mode else "target"
win = Gtk.Window(title="Glass cross-window " + name)
win.set_decorated(False)
win.set_default_size(width, 300)
win.set_position(Gtk.WindowPosition.CENTER)
button = Gtk.Button(label="Cover action" if cover_mode else "Covered action")
win.add(button)
count = 0
state = None


def publish(filename, value):
    path = folder / filename
    temporary = path.with_suffix(".tmp")
    temporary.write_text(str(value))
    temporary.replace(path)


def clicked(_button):
    global count
    count += 1
    publish(name + "-count", count)


def update():
    global state
    command = (folder / "command").read_text().strip()
    if cover_mode:
        wanted = "pass" if command == "pass" else "cover"
        if wanted != state:
            if wanted == "pass":
                win.get_window().input_shape_combine_region(cairo.Region(), 0, 0)
            elif state == "pass":
                region = cairo.Region(cairo.RectangleInt(0, 0, width, 300))
                win.get_window().input_shape_combine_region(region, 0, 0)
            win.get_display().sync()
            state = wanted
            publish("cover-state", state)
    return True


assert os.environ.get("DISPLAY"), "this fixture requires its test's X11 display"
button.connect("clicked", clicked)
win.connect("destroy", Gtk.main_quit)
win.show_all()
publish(name + "-count", count)
if cover_mode:
    GLib.timeout_add(20, update)
else:
    keys = ("DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS", "XDG_RUNTIME_DIR")
    publish("environment", "\n".join(k + "=" + os.environ[k] for k in keys if k in os.environ))
Gtk.main()
