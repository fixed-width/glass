import os

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gio, Gtk


def on_map(window):
    renderer = window.get_renderer()
    kind = renderer.__gtype__.name if renderer is not None else "none"
    print(f"RENDERER requested={os.environ.get('GSK_RENDERER')} actual={kind}", flush=True)


def activate(app):
    window = Gtk.ApplicationWindow(application=app, title="Glass renderer probe")
    window.set_default_size(320, 240)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
    box.append(Gtk.Label(label="Renderer probe"))
    box.append(Gtk.Button(label="Apply"))
    window.set_child(box)
    window.connect("map", on_map)
    window.present()


app = Gtk.Application(
    application_id="tech.fixedwidth.glass.rendererprobe",
    flags=Gio.ApplicationFlags.NON_UNIQUE,
)
app.connect("activate", activate)
app.run(None)
