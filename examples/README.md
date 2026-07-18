# examples

Small apps for trying glass's **build → see → interact → debug** loop with an agent.

## `tasks_demo.py` — a GTK4 app with a bug to find

A tiny "Tasks" app: a text entry, an **Add** button, and a list. It ships with a deliberate bug —
clicking **Add** does nothing — so you can watch an agent drive glass through the whole loop: run
the app, reproduce the bug **from the accessibility tree** (no screenshots), fix the code, and
verify the fix.

Requires Python 3 with GTK 4 introspection:

```bash
# Debian / Ubuntu
sudo apt install python3-gi gir1.2-gtk-4.0
```

[Connect glass to your agent](../docs/how-to/connect-an-agent.md), then paste:

> Use glass to run `examples/tasks_demo.py` with accessibility on. There's a bug: clicking
> **Add** doesn't add the typed task. Reproduce it by driving the UI and checking the
> accessibility tree (don't just screenshot), then find and fix the bug in the code and verify a
> task actually appears.

A good run launches with `a11y: true`, types a task, clicks **Add**, and sees from
`glass_a11y_snapshot` that the list is still empty — then, after the fix, that a new list item
appears. The app edits the agent makes are to `tasks_demo.py`; `git checkout examples/tasks_demo.py`
resets it to the buggy version to run the demo again.

<details>
<summary>The answer (spoiler)</summary>

The `on_add` handler is correct, but the **Add** button is never connected to it. The one-line fix:

```python
add = Gtk.Button(label="Add")
add.connect("clicked", self.on_add)   # <- the missing line
```

</details>
