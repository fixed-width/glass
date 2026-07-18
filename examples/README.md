# examples

Small apps for trying glass's **build → see → interact → debug** loop with an agent.

## `tasks_demo.py` — a GTK4 app with a bug to find

A tiny "Tasks" app: a text entry, an **Add** button, and a list. It has a bug — clicking **Add**
doesn't add the typed task — so you can watch an agent drive glass through the whole loop: run the
app, reproduce the bug **from the accessibility tree** (no screenshots), find and fix it in the
code, and verify.

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

Let the agent find it — the fix is a single line. Run `git checkout examples/tasks_demo.py` to
reset the app to its buggy state and try again.
