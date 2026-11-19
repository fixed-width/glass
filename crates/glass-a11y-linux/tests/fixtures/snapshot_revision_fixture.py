#!/usr/bin/env python3
"""GTK4 fixture for checking application-wide snapshot revisions on Linux."""

import os
import subprocess
import sys

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gio, GLib, Gtk


def report_peer_registration(bus):
    try:
        (roots,) = bus.call_sync(
            "org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root",
            "org.a11y.atspi.Accessible", "GetChildren", None, None,
            Gio.DBusCallFlags.NONE, 200, None,
        ).unpack()
        for name, _ in roots:
            (pid,) = bus.call_sync(
                "org.freedesktop.DBus", "/org/freedesktop/DBus",
                "org.freedesktop.DBus", "GetConnectionUnixProcessID",
                GLib.Variant("(s)", (name,)), None, Gio.DBusCallFlags.NONE, 200, None,
            ).unpack()
            if pid == os.getpid():
                print("REVISION_PEER_REGISTERED", flush=True)
                return GLib.SOURCE_REMOVE
    except GLib.Error:
        pass
    return GLib.SOURCE_CONTINUE


class Fixture(Gtk.Application):
    def __init__(self, peer):
        super().__init__(flags=Gio.ApplicationFlags.NON_UNIQUE)
        self.peer = peer
        self.peer_process = None

    def do_activate(self):
        if self.peer:
            self.window("Revision peer", Gtk.Label(label="Independent provider"))
            bus = Gio.DBusConnection.new_for_address_sync(
                os.environ["AT_SPI_BUS_ADDRESS"],
                Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
                | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
                None, None,
            )
            GLib.timeout_add(50, report_peer_registration, bus)
            return

        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        for label, initial in [("Revision target", "initial"), ("Revision decoy", "alternate")]:
            entry = Gtk.Entry(text=initial)
            entry.update_property([Gtk.AccessibleProperty.LABEL], [label])
            content.append(entry)
        entry.connect("changed", self.start_peer)
        for number in range(150):
            content.append(Gtk.Label(label=f"Stable revision fixture row {number:03}"))
        scroll = Gtk.ScrolledWindow(child=content)
        self.window("Revision main", scroll)
        self.window("Revision sibling", Gtk.Label(label="Sibling window content"))

    def window(self, title, child):
        window = Gtk.ApplicationWindow(application=self, title=title, child=child)
        window.set_default_size(420, 300)
        window.present()

    def start_peer(self, entry):
        if entry.get_text() == "launch-peer" and self.peer_process is None:
            self.peer_process = subprocess.Popen([sys.executable, __file__, "--peer"])


if __name__ == "__main__":
    Fixture("--peer" in sys.argv).run(sys.argv[:1])
