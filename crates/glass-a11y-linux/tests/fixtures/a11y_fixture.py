#!/usr/bin/env python3
"""Minimal GTK4 app with a known accessibility tree, for glass-a11y-linux tests.
Window "Glass A11y Fixture" containing a Label "Ready", a Button "Save", a
CheckButton "Enable", and an Entry "Field" (initial text "hello"). Run by
scripts/test-a11y.sh via glass (which sets DISPLAY).

Uses Gio.ApplicationFlags.NON_UNIQUE so the app skips D-Bus singleton registration
and presents its window immediately without waiting for portal services to settle."""
import sys
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gio", "2.0")
from gi.repository import Gio, Gtk  # noqa: E402


class FixtureApp(Gtk.Application):
    def __init__(self):
        super().__init__(
            application_id="net.jesterscourt.GlassA11yFixture",
            flags=Gio.ApplicationFlags.NON_UNIQUE,
        )

    def do_activate(self):
        win = Gtk.ApplicationWindow(application=self, title="Glass A11y Fixture")
        win.set_default_size(320, 200)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.append(Gtk.Label(label="Ready"))
        box.append(Gtk.Button(label="Save"))
        box.append(Gtk.CheckButton(label="Enable"))
        entry = Gtk.Entry()
        entry.set_text("hello")
        entry.update_property([Gtk.AccessibleProperty.LABEL], ["Field"])
        box.append(entry)
        win.set_child(box)
        win.present()


if __name__ == "__main__":
    FixtureApp().run(sys.argv)
