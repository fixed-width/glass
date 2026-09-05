#!/usr/bin/env python3
"""Minimal GTK4 app with a known accessibility tree, for glass-a11y-linux tests.
Window "Glass A11y Fixture" containing a Label "Ready", a Button "Save", a
Button "Bold" (AT-SPI description "Bold text", distinct from its name), a
Button "Italic" (AT-SPI description "Italic", the SAME string as its name), a
CheckButton "Enable", an Entry "Field" (initial text "hello"), a SpinButton
"Amount" (initial value 1), a DropDown "Company" (Acme/Globex/Initech), a
Switch "Active" (off), a virtualized GtkListView of 80 rows ("Row 000".."Row
079") in a small scroller, a Scale "Volume", a ProgressBar "Progress", a
horizontal Separator, a Toolbar containing a Button "Cut", a LinkButton "Docs",
and a Notebook with two pages ("One", "Two"). Run by scripts/test-a11y.sh via
glass (which sets DISPLAY).

Uses Gio.ApplicationFlags.NON_UNIQUE so the app skips D-Bus singleton registration
and presents its window immediately without waiting for portal services to settle."""
import sys
import gi

GTK3_FOCUS_MODE = "--gtk3-focus" in sys.argv

gi.require_version("Gtk", "3.0" if GTK3_FOCUS_MODE else "4.0")
gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib, Gtk  # noqa: E402

if GTK3_FOCUS_MODE:
    gi.require_version("Atk", "1.0")
    from gi.repository import Atk  # noqa: E402


class FocusFixtureApp(Gtk.Application):
    def __init__(self):
        super().__init__(
            application_id="net.jesterscourt.GlassA11yFocusFixture",
            flags=Gio.ApplicationFlags.NON_UNIQUE,
        )

    def do_activate(self):
        win = Gtk.ApplicationWindow(
            application=self, title="Glass A11y Focus Fixture"
        )
        win.set_default_size(320, 120)
        entry = Gtk.Entry()
        entry.set_text("hello")
        accessible = entry.get_accessible()
        accessible.set_name("Field")
        accessible.set_role(Atk.Role.ENTRY)
        win.add(entry)
        win.show_all()


class FixtureApp(Gtk.Application):
    def __init__(self):
        super().__init__(
            application_id="net.jesterscourt.GlassA11yFixture",
            flags=Gio.ApplicationFlags.NON_UNIQUE,
        )

    def do_activate(self):
        win = Gtk.ApplicationWindow(application=self, title="Glass A11y Fixture")
        win.set_default_size(320, 420)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.append(Gtk.Label(label="Ready"))

        moving = Gtk.Button(label="Moving semantic")
        moving.connect(
            "clicked", lambda _button: print("MOVING_CLICKED", flush=True)
        )

        def _start_moving(_button):
            print("SEMANTIC_SAVE", flush=True)
            started = GLib.get_monotonic_time()

            def _move():
                elapsed_ms = (GLib.get_monotonic_time() - started) // 1000
                if elapsed_ms >= 300:
                    print("MOVING_SETTLED", flush=True)
                    return GLib.SOURCE_REMOVE
                moving.set_margin_start((elapsed_ms // 30) * 4)
                return GLib.SOURCE_CONTINUE

            GLib.timeout_add(30, _move)

        save_semantic = Gtk.Button(label="Semantic Save")
        save_semantic.connect("clicked", _start_moving)

        blocked = Gtk.Button(label="Disabled semantic")
        blocked.set_sensitive(False)

        duplicate_one = Gtk.Button(label="Duplicate semantic")
        duplicate_two = Gtk.Button(label="Duplicate semantic")

        occlusion_stage = Gtk.Overlay()
        occluded = Gtk.Button(label="Occluded semantic")
        occluded.set_hexpand(True)
        occlusion_stage.set_child(occluded)
        occluder = Gtk.Button(label="Occluder")
        occluder.set_halign(Gtk.Align.CENTER)
        occluder.set_valign(Gtk.Align.CENTER)
        occlusion_stage.add_overlay(occluder)

        semantic_grid = Gtk.Grid(row_spacing=4, column_spacing=8)
        semantic_grid.attach(save_semantic, 0, 0, 1, 1)
        semantic_grid.attach(blocked, 1, 0, 1, 1)
        semantic_grid.attach(duplicate_one, 0, 1, 1, 1)
        semantic_grid.attach(duplicate_two, 1, 1, 1, 1)
        semantic_grid.attach(moving, 0, 2, 1, 1)
        semantic_grid.attach(occlusion_stage, 1, 2, 1, 1)
        box.append(semantic_grid)

        box.append(Gtk.Button(label="Save"))
        bold = Gtk.Button(label="Bold")
        # update_property(DESCRIPTION) populates AT-SPI Description (verified on the bus).
        bold.update_property([Gtk.AccessibleProperty.DESCRIPTION], ["Bold text"])
        box.append(bold)
        italic = Gtk.Button(label="Italic")
        # Description == name, the case normalize_description drops: toolkits routinely
        # report one label in both fields, and the outline must not print it twice.
        italic.update_property([Gtk.AccessibleProperty.DESCRIPTION], ["Italic"])
        box.append(italic)
        box.append(Gtk.CheckButton(label="Enable"))
        entry = Gtk.Entry()
        entry.set_text("hello")
        entry.update_property([Gtk.AccessibleProperty.LABEL], ["Field"])
        box.append(entry)
        unconfirmed = Gtk.Entry()
        unconfirmed.set_text("untouched")
        unconfirmed.set_focus_on_click(False)

        def _restore_focusable():
            unconfirmed.set_can_focus(True)
            return GLib.SOURCE_REMOVE

        def _refuse_pointer_focus(_gesture, _presses, _x, _y):
            unconfirmed.set_can_focus(False)
            win.set_focus(save_semantic)
            GLib.timeout_add(300, _restore_focusable)

        refusal_gesture = Gtk.GestureClick()
        refusal_gesture.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        refusal_gesture.connect("pressed", _refuse_pointer_focus)
        unconfirmed.add_controller(refusal_gesture)
        unconfirmed.update_property(
            [Gtk.AccessibleProperty.LABEL], ["Refusing Editor"]
        )
        box.append(unconfirmed)
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
        # A virtualized GtkListView of 80 rows in a small scroller. GtkListView
        # only realizes rows near the viewport, so a late row ("Row 060") is ABSENT
        # from the a11y tree until scrolled into view — the case scroll_to_element
        # must overcome (a non-virtualizing GtkListBox would realize all 80 rows and
        # let a test pass without scrolling). On selection it prints "SELECTED <name>"
        # so a click can be confirmed from the logs regardless of where GTK places the
        # selected state in the a11y tree.
        rows = Gtk.StringList.new([f"Row {i:03d}" for i in range(80)])
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

        # Widgets added for the role-coverage test. Labels are deliberately distinct from the
        # existing fixture widgets so name-based lookups in other tests stay unambiguous.
        # Accessible names go through update_property, the GTK4 idiom already used above —
        # GTK3's get_accessible()/ATK path does not exist in GTK4.
        scale = Gtk.Scale(
            orientation=Gtk.Orientation.HORIZONTAL,
            adjustment=Gtk.Adjustment(value=25, lower=0, upper=100, step_increment=1),
        )
        scale.update_property([Gtk.AccessibleProperty.LABEL], ["Volume"])
        box.append(scale)

        progress = Gtk.ProgressBar()
        progress.set_fraction(0.4)
        progress.update_property([Gtk.AccessibleProperty.LABEL], ["Progress"])
        box.append(progress)

        box.append(Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL))

        # A plain box carrying the TOOLBAR accessible role — GTK4 sets the role at
        # construction, so it cannot be changed after the widget exists.
        toolbar = Gtk.Box(
            orientation=Gtk.Orientation.HORIZONTAL,
            accessible_role=Gtk.AccessibleRole.TOOLBAR,
        )
        toolbar.append(Gtk.Button(label="Cut"))
        box.append(toolbar)

        link = Gtk.LinkButton(uri="https://example.invalid", label="Docs")
        box.append(link)

        notebook = Gtk.Notebook()
        notebook.append_page(Gtk.Label(label="first page"), Gtk.Label(label="One"))
        notebook.append_page(Gtk.Label(label="second page"), Gtk.Label(label="Two"))
        box.append(notebook)

        win.set_child(box)
        win.present()


if __name__ == "__main__":
    app_args = [arg for arg in sys.argv if arg != "--gtk3-focus"]
    (FocusFixtureApp() if GTK3_FOCUS_MODE else FixtureApp()).run(app_args)
