"""Application-boundary recipes and independent participant-aware outcomes."""

import re
import time

CASES = {
    name: "task_completed"
    for name in (
        "electron-form",
        "android-boundary",
        "native-form",
        "ios-publication",
        "cross-application",
    )
}
ANDROID_NAMES = {
    "Account name": "account",
    "Saved value": "saved",
    "Submission count": "count",
}


def form(driver, value):
    driver.find("discover_field", "Account name", "TextField")
    driver.find("discover_save", "Save account")
    driver.read("empty", "Account name", "")
    driver.read("before_count", "Submission count", "0")
    driver.type("type", "Account name", value)
    driver.read("typed", "Account name", value)
    driver.click("action", "Save account")
    driver.read("saved", "Saved value", value)
    driver.read("count", "Submission count", "1")
    time.sleep(0.35)
    driver.read("quiet_count", "Submission count", "1")


def execute(case, fleet):
    if case == "ios-publication":
        from ios_publication import execute as probe

        probe(fleet.members["app"])
        return
    if case == "cross-application":
        source, destination = fleet.members["source"], fleet.members["destination"]
        source.read("source_empty", "Source value", "")
        source.read("before_generation", "Generation count", "0")
        source.click("generate", "Generate transfer")
        source.read("generation", "Generation count", "1")
        observed = source.read("source_value", "Source value")
        if len(observed["nodes"]) != 1 or not observed["nodes"][0].get("value"):
            raise ValueError("source did not expose a transfer value")
        form(destination, observed["nodes"][0]["value"])
        source.read("quiet_generation", "Generation count", "1")
        source.read("quiet_source", "Source value", observed["nodes"][0]["value"])
        return
    app = fleet.members["app"]
    if case == "android-boundary":
        app.click("open_form", "Open form")
        app.label("form_stage", "Native stage: form")
        app.read("web_ready", "Account name", "")
    form(app, "Ada")
    if case == "electron-form":
        app.read("before_confirmation", "Confirmation count", "0")
        app.click("review", "Review saved value")
        app.confirm_dialog()
        app.read("confirmed", "Confirmed value", "Ada")
        app.read("confirmation_count", "Confirmation count", "1")
        time.sleep(0.35)
        app.read("quiet_confirmation", "Confirmation count", "1")
    elif case == "android-boundary":
        app.click("review", "Review saved value")
        app.label("review_stage", "Native stage: review")
        app.label("native_saved", "Native saved value: Ada")
        app.label("native_count", "Native submission count: 1")
        app.label("native_reviews", "Native review count: 1")
        time.sleep(0.35)
        app.label("quiet_native_count", "Native submission count: 1")
        app.label("quiet_native_reviews", "Native review count: 1")


class Oracle:
    def __init__(self, events):
        self.facts = {(e["participant"], e["step"]): e["facts"] for e in events}
        self.order = {(e["participant"], e["step"]): i for i, e in enumerate(events)}
        self.errors = (
            []
            if len(self.facts) == len(events)
            else ["duplicate participant/evidence step"]
        )

    def fact(self, step, participant="app"):
        result = self.facts.get((participant, step), {})
        if not result:
            self.errors.append(f"{step}: missing participant {participant} evidence")
        return result

    def sequence(self, steps):
        positions = [self.order.get(step, -1) for step in steps]
        if -1 in positions or positions != sorted(set(positions)):
            self.errors.append(
                "required observations/actions are missing or out of order"
            )

    def value(self, step, name, value, participant="app"):
        fact = self.fact(step, participant)
        nodes = fact.get("nodes", [])
        if (
            step == "empty"
            and name == "Account name"
            and value == ""
            and fact.get("error") is False
            and fact.get("set_value")
            == {"target": {"query": name, "role": "TextField"}, "text": ""}
            and fact.get("result", {}).get("dispatch") == "dispatched"
            and fact.get("result", {}).get("confirmation") == "value_confirmed"
        ):
            return
        if (
            fact.get("error") is not False
            or len(nodes) != 1
            or not (
                nodes[0].get("name") == name
                or (
                    nodes[0].get("name") == ANDROID_NAMES.get(name)
                    and nodes[0].get("description") == name
                )
            )
            or nodes[0].get("value") != value
        ):
            self.errors.append(f"{step}: expected {name} = {value!r}")

    def label(self, step, name):
        self.value(step, name, None)

    def action(self, step, participant="app", typing=False):
        fact = self.fact(step, participant)
        result = fact.get("result", {})
        key = "type_dispatch" if typing else "dispatch"
        if fact.get("error") is not False or result.get(key) != "dispatched":
            self.errors.append(f"{step}: no successful dispatch")
        if typing and result.get("focus_confirmation") != "focus_confirmed":
            self.errors.append(f"{step}: focus was not confirmed")

    def discovery(self, step, name, participant="app"):
        fact = self.fact(step, participant)
        nodes = fact.get("nodes", [])
        if (
            fact.get("error") is not False
            or fact.get("result", {}).get("search_complete") is not True
            or len(nodes) != 1
            or name not in (nodes[0].get("name"), nodes[0].get("description"))
        ):
            self.errors.append(f"{step}: no unique complete discovery of {name}")


def evaluate_setup(case, events, viewport):
    if case == "ios-publication":
        return []
    oracle = Oracle(events)
    if case == "android-boundary":
        oracle.label("entry", "Native stage: entry")
    else:
        for participant in (
            ("source", "destination") if case == "cross-application" else ("app",)
        ):
            oracle.value("ready", "Fixture ready", "ready", participant)
            if case == "electron-form" or participant == "destination":
                oracle.value(
                    "geometry",
                    "Content geometry",
                    f"{viewport[0]}x{viewport[1]}@1",
                    participant,
                )
            else:
                geometry = oracle.fact("geometry", participant)
                bounds = geometry.get("result", {})
                if geometry.get("error") is not False or (
                    bounds.get("width"),
                    bounds.get("height"),
                ) != (600, 500):
                    oracle.errors.append("native fixture geometry differs from 600x500")
    return oracle.errors


def evaluate(case, events):
    oracle = Oracle(events)
    participant, value = "app", "Ada"
    if case == "cross-application":
        participant = "destination"
        fact = oracle.fact("source_value", "source")
        nodes = fact.get("nodes", [])
        value = nodes[0].get("value") if len(nodes) == 1 else None
        if not isinstance(value, str) or not re.fullmatch(
            r"ticket-\d+-[a-f0-9]{32}", value
        ):
            oracle.errors.append("source_value: no app-generated transfer value")
        oracle.value("source_empty", "Source value", "", "source")
        oracle.value("before_generation", "Generation count", "0", "source")
        oracle.action("generate", "source")
        oracle.value("source_value", "Source value", value, "source")
        oracle.value("generation", "Generation count", "1", "source")
        oracle.value("quiet_generation", "Generation count", "1", "source")
        oracle.value("quiet_source", "Source value", value, "source")
    oracle.discovery("discover_field", "Account name", participant)
    oracle.discovery("discover_save", "Save account", participant)
    oracle.value("empty", "Account name", "", participant)
    oracle.value("before_count", "Submission count", "0", participant)
    oracle.action("type", participant, typing=True)
    oracle.value("typed", "Account name", value, participant)
    oracle.action("action", participant)
    oracle.value("saved", "Saved value", value, participant)
    oracle.value("count", "Submission count", "1", participant)
    oracle.value("quiet_count", "Submission count", "1", participant)
    oracle.sequence(
        [
            (participant, step)
            for step in (
                "discover_field",
                "discover_save",
                "empty",
                "before_count",
                "type",
                "typed",
                "action",
                "saved",
                "count",
                "quiet_count",
            )
        ]
    )
    if case == "cross-application":
        oracle.sequence(
            [
                ("source", step)
                for step in (
                    "source_empty",
                    "before_generation",
                    "generate",
                    "generation",
                    "source_value",
                )
            ]
            + [
                ("destination", "type"),
                ("destination", "quiet_count"),
                ("source", "quiet_generation"),
                ("source", "quiet_source"),
            ]
        )
    if case == "android-boundary":
        oracle.sequence(
            [
                ("app", step)
                for step in (
                    "entry",
                    "open_form",
                    "form_stage",
                    "web_ready",
                    "type",
                    "quiet_count",
                    "review",
                    "review_stage",
                    "native_saved",
                    "native_count",
                    "native_reviews",
                    "quiet_native_count",
                    "quiet_native_reviews",
                )
            ]
        )
        oracle.label("entry", "Native stage: entry")
        oracle.action("open_form")
        oracle.label("form_stage", "Native stage: form")
        oracle.value("web_ready", "Account name", "")
        oracle.action("review")
        oracle.label("review_stage", "Native stage: review")
        for step, name in (
            ("native_saved", "Native saved value: Ada"),
            ("native_count", "Native submission count: 1"),
            ("native_reviews", "Native review count: 1"),
            ("quiet_native_count", "Native submission count: 1"),
            ("quiet_native_reviews", "Native review count: 1"),
        ):
            oracle.label(step, name)
    elif case == "electron-form":
        oracle.sequence(
            [
                ("app", step)
                for step in (
                    "quiet_count",
                    "before_confirmation",
                    "review",
                    "dialog_open",
                    "dialog_select",
                    "dialog_focus",
                    "dialog_confirm",
                    "dialog_closed",
                    "main_select",
                    "confirmed",
                    "confirmation_count",
                    "quiet_confirmation",
                )
            ]
        )
        oracle.action("review")
        windows = oracle.fact("dialog_open").get("windows", [])
        dialogs = [w for w in windows if w.get("title") == "Confirm account"]
        selected = oracle.fact("dialog_select")
        if (
            len(dialogs) != 1
            or selected.get("error") is not False
            or selected.get("selected_window") != dialogs[0]["id"]
        ):
            oracle.errors.append("dialog: native window was not observed and selected")
        confirm = oracle.fact("dialog_confirm")
        if oracle.fact("dialog_focus").get("error") is not False:
            oracle.errors.append("dialog: selected window could not be focused")
        if confirm.get("error") is not False or confirm.get("key") != "Return":
            oracle.errors.append("dialog: default confirmation key was not delivered")
        closed = oracle.fact("dialog_closed")
        if (
            closed.get("error") is not False
            or not closed.get("windows")
            or any(w.get("title") == "Confirm account" for w in closed["windows"])
        ):
            oracle.errors.append("dialog: closure was not observed")
        main = oracle.fact("main_select")
        remaining = closed.get("windows", [])
        if (
            len(remaining) != 1
            or main.get("error") is not False
            or main.get("selected_window") != remaining[0]["id"]
        ):
            oracle.errors.append("dialog: main window was not reselected")
        oracle.value("before_confirmation", "Confirmation count", "0")
        oracle.value("confirmed", "Confirmed value", "Ada")
        oracle.value("confirmation_count", "Confirmation count", "1")
        oracle.value("quiet_confirmation", "Confirmation count", "1")
    return oracle.errors
