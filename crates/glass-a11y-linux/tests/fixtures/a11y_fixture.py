#!/usr/bin/env python3
"""Minimal GTK4 app with a known accessibility tree, for glass-a11y-linux tests.
Window "Glass A11y Fixture" containing a Label "Ready", a Button "Save", a
CheckButton "Enable", an Entry "Field" (initial text "hello"), a SpinButton
"Amount" (initial value 1), a DropDown "Company" (Acme/Globex/Initech), a
Switch "Active" (off), and a virtualized GtkListView of 200 rows ("Row 000".."Row
199") in a small scroller. Run by scripts/test-a11y.sh via glass (which sets DISPLAY).

Uses Gio.ApplicationFlags.NON_UNIQUE so the app skips D-Bus singleton registration
and presents its window immediately without waiting for portal services to settle."""
import sys
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gio", "2.0")
from gi.repository import Gdk, Gio, Gtk  # noqa: E402


class FixtureApp(Gtk.Application):
    def __init__(self):
        super().__init__(
            application_id="net.jesterscourt.GlassA11yFixture",
            flags=Gio.ApplicationFlags.NON_UNIQUE,
        )

    def do_activate(self):
        # GtkScrolledWindow paints a thin "undershoot/overshoot" shadow at a scroll
        # boundary, and repaints it (with a pixel or two of antialiasing jitter)
        # every time a further scroll is attempted past that boundary — even
        # though the adjustment's value itself is fully clamped and unchanging.
        # scroll_to_element treats any such pixel change as "still moving", so
        # left alone this defeats its bidirectional sweep's own end-of-scroll
        # detection: it would never see "no motion" and so never reverse
        # direction. Suppress the indicator so a clamped scroll is visually a
        # true no-op.
        css = Gtk.CssProvider()
        css.load_from_string(
            "scrolledwindow > undershoot, scrolledwindow > overshoot { "
            "background: none; background-image: none; box-shadow: none; }"
        )
        Gtk.StyleContext.add_provider_for_display(
            Gdk.Display.get_default(),
            css,
            Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION,
        )
        win = Gtk.ApplicationWindow(application=self, title="Glass A11y Fixture")
        win.set_default_size(320, 420)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.append(Gtk.Label(label="Ready"))
        box.append(Gtk.Button(label="Save"))
        box.append(Gtk.CheckButton(label="Enable"))
        entry = Gtk.Entry()
        entry.set_text("hello")
        entry.update_property([Gtk.AccessibleProperty.LABEL], ["Field"])
        box.append(entry)
        # A SpinButton exposes BOTH the AT-SPI EditableText and Value interfaces; only
        # Value commits to the adjustment, so set_value must go through Value.
        spin = Gtk.SpinButton(
            adjustment=Gtk.Adjustment(value=1, lower=0, upper=10, step_increment=1),
            digits=0,
        )
        spin.update_property([Gtk.AccessibleProperty.LABEL], ["Amount"])
        box.append(spin)
        # A GtkDropDown. Its options only commit on row activation (Enter/click), not
        # via AT-SPI SelectChild, so glass drives it with the keyboard. Starts on
        # "Acme" (index 0).
        dropdown = Gtk.DropDown.new_from_strings(["Acme", "Globex", "Initech"])
        dropdown.update_property([Gtk.AccessibleProperty.LABEL], ["Company"])
        box.append(dropdown)
        # A GtkSwitch exposes the AT-SPI Action interface + a boolean CHECKED state;
        # set_value should toggle it to a target boolean. Starts off.
        switch = Gtk.Switch()
        switch.set_halign(Gtk.Align.START)
        switch.update_property([Gtk.AccessibleProperty.LABEL], ["Active"])
        box.append(switch)
        # A virtualized GtkListView of 200 rows in a small scroller. GtkListView
        # only realizes rows near the viewport, so a late row ("Row 180") is ABSENT
        # from the a11y tree until scrolled into view — the case scroll_to_element
        # must overcome (a non-virtualizing GtkListBox would realize all 200 rows and
        # let a test pass without scrolling). On selection it prints "SELECTED <name>"
        # so a click can be confirmed from the logs regardless of where GTK places the
        # selected state in the a11y tree.
        rows = Gtk.StringList.new([f"Row {i:03d}" for i in range(200)])
        selection = Gtk.SingleSelection(model=rows)
        selection.set_autoselect(False)
        selection.set_can_unselect(True)
        selection.set_selected(Gtk.INVALID_LIST_POSITION)

        def _on_selection_changed(sel, _pos, _n_items):
            i = sel.get_selected()
            if i != Gtk.INVALID_LIST_POSITION:
                print(f"SELECTED {rows.get_string(i)}", flush=True)

        selection.connect("selection-changed", _on_selection_changed)
        factory = Gtk.SignalListItemFactory()
        factory.connect("setup", lambda _f, item: item.set_child(Gtk.Label()))
        factory.connect(
            "bind",
            lambda _f, item: item.get_child().set_text(item.get_item().get_string()),
        )
        listview = Gtk.ListView(model=selection, factory=factory)
        scroller = Gtk.ScrolledWindow()
        scroller.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        scroller.set_min_content_height(120)
        scroller.set_max_content_height(120)
        scroller.set_child(listview)
        box.append(scroller)
        win.set_child(box)
        win.present()


if __name__ == "__main__":
    FixtureApp().run(sys.argv)
