"""A tiny GTK4 "Tasks" app — with a deliberate bug — for trying glass end to end.

There's a bug: clicking **Add** does nothing. Point an agent at glass and have it run this app,
reproduce the bug from the accessibility tree, find and fix it in the code, and verify a task
appears. See examples/README.md for the prompt (and, if you want it, the answer).

Run directly to see the bug yourself:  python3 examples/tasks_demo.py
(needs Python 3 + GTK 4 introspection: `apt install python3-gi gir1.2-gtk-4.0`)
"""

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gtk


class TaskWindow(Gtk.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title="Tasks")
        self.set_default_size(360, 420)

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        for margin in ("top", "bottom", "start", "end"):
            getattr(box, f"set_margin_{margin}")(12)
        self.set_child(box)

        input_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        self.entry = Gtk.Entry()
        self.entry.set_placeholder_text("New task")
        self.entry.set_hexpand(True)
        add = Gtk.Button(label="Add")
        input_row.append(self.entry)
        input_row.append(add)
        box.append(input_row)

        self.tasks = Gtk.ListBox()
        box.append(self.tasks)

    def on_add(self, _button):
        text = self.entry.get_text().strip()
        if not text:
            return
        self.tasks.append(Gtk.Label(label=text, xalign=0))
        self.entry.set_text("")


def main():
    app = Gtk.Application(application_id="tech.fixedwidth.glass.tasks")
    app.connect("activate", lambda a: TaskWindow(a).present())
    app.run(None)


if __name__ == "__main__":
    main()
