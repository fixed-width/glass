"""Exercise MCP snapshot revisions from an interactive Windows session."""

import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import time

from evidence import Evidence
from protocol import Client
from snapshot_revisions import SnapshotReceiver


REVISION = "glass_a11y_snapshot_diff"
TITLE = "Glass Revision Main"
FIXTURE = (
    Path(__file__).resolve().parents[2]
    / "crates/glass-windows/tests/fixtures/snapshot_revision_fixture.ps1"
)
POWERSHELL = ["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass"]


def qualify(client, evidence):
    receiver = SnapshotReceiver()

    def call(tool, *, error=False, **arguments):
        reply = client.rpc("tools/call", {"name": tool, "arguments": arguments})
        decoded = evidence.decode(tool, reply)
        assert decoded["is_error"] == error, decoded["texts"]
        return decoded

    def revision(previous=None, *, kind, cacheable=True, max_nodes=None):
        base = previous["revision"] if previous else None
        sequence = client.sequence + 1
        receiver.begin(sequence, base)
        current = receiver.accept(
            sequence, call(REVISION, base_revision=base, max_nodes=max_nodes)
        )
        assert current["kind"] == kind and current["cacheable"] == cacheable, current
        fresh = call("glass_a11y_snapshot", max_nodes=max_nodes)["observations"][0]
        assert fresh == current["outline"]
        last_id = int(fresh.splitlines()[-1].split()[0][1:])
        assert last_id + 1 == current["completeness"]["count"]
        return current

    def find_window(title, previous=None):
        end = time.monotonic() + 8
        while time.monotonic() < end:
            windows = call("glass_list_windows")["observations"][0]
            if previous is None or all(window["id"] != previous for window in windows):
                for window in windows:
                    if window["title"] == title:
                        return window
            time.sleep(0.05)
        raise AssertionError(f"Window did not appear: {title}")

    def select(title, previous=None):
        window = find_window(title, previous)
        call("glass_select_window", id=window["id"])
        return window["id"]

    def start():
        call(
            "glass_start",
            run=[*POWERSHELL, "-File", str(FIXTURE)],
            window_hint={"title": TITLE},
            backend="windows",
            sandbox="off",
            a11y=True,
        )
        return select(TITLE)

    def stop():
        owned = set(client.job.pids()) - {client.process.pid}
        assert owned, "Fixture process was not observed in the client Job"
        call("glass_stop")
        assert not (owned & set(client.job.pids())), "Owned fixture survived glass_stop"

    def field_line(observation, field):
        marker = f' TextField "{field}" '
        [line] = [line for line in observation["outline"].splitlines() if marker in line]
        return line

    def set_text(observation, field, text):
        line = field_line(observation, field)
        call("glass_set_value", id=int(line.split()[0][1:]), text=text)

    def expect_values(observation, target, decoy):
        for field, value in [("Revision target", target), ("Revision decoy", decoy)]:
            line = field_line(observation, field)
            assert f' TextField "{field}" value="{value}" ' in line, line

    main = start()
    baseline = revision(kind="full")
    assert "Main window content" in baseline["outline"]
    assert "Sibling window content" not in baseline["outline"]
    stable = revision(baseline, kind="unchanged")

    set_text(stable, "Revision target", "initial")
    noop = revision(stable, kind="unchanged")
    expect_values(noop, "initial", "initial")
    set_text(noop, "Revision decoy", "wrong-field")
    wrong = revision(noop, kind="diff")
    expect_values(wrong, "initial", "wrong-field")
    set_text(wrong, "Revision target", "confirm-é")
    changed = revision(wrong, kind="diff")
    expect_values(changed, "confirm-é", "wrong-field")

    wait = call("glass_wait_for_element", name="Revision target", timeout_ms=1000)
    assert wait["envelope"]["result"]["matched"]
    after_wait = revision(changed, kind="full")
    assert after_wait["context_id"] != changed["context_id"]

    call(REVISION, base_revision=after_wait["revision"])
    recovered = revision(after_wait, kind="full")
    assert recovered["full_reason"] == "baseline_unavailable"
    receiver = SnapshotReceiver()
    recovered = revision(kind="full")
    assert recovered["full_reason"] == "requested"

    sibling = select(f"{TITLE} Sibling")
    switched = revision(recovered, kind="full")
    assert "Sibling window content" in switched["outline"]
    assert "Main window content" not in switched["outline"]
    assert switched["context_id"] != recovered["context_id"]
    call("glass_select_window", id=sibling)
    reselected = revision(switched, kind="full")
    assert reselected["context_id"] != switched["context_id"]
    call("glass_window", op="resize", width=510, height=480)
    resized = revision(reselected, kind="full")
    assert resized["context_id"] != reselected["context_id"]
    limited = revision(resized, kind="full", max_nodes=1)
    assert limited["context_id"] != resized["context_id"]
    assert limited["completeness"]["truncated"]["limit"] == "nodes"

    offscreen = call("glass_window", op="move", x=-20000, y=-20000)
    assert offscreen["envelope"]["result"]["x"] < -19000
    outside = revision(limited, kind="full", cacheable=False)
    assert outside["full_reason"] == "context_unproven"

    call("glass_select_window", id=main)
    before_replace = revision(outside, kind="full")
    call("glass_click_element", target={"query": "Replace window"})
    replacement = find_window(TITLE, main)["id"]
    call(REVISION, error=True, base_revision=before_replace["revision"])
    call("glass_select_window", id=replacement)
    replaced = revision(before_replace, kind="full")
    assert replaced["context_id"] != before_replace["context_id"]

    stop()
    start()
    restarted = revision(replaced, kind="full")
    assert restarted["context_id"] != replaced["context_id"]
    stop()

    external_title = f"Glass External Revision {os.getpid()}"
    with subprocess.Popen(
        [*POWERSHELL, "-File", str(FIXTURE), "-Title", external_title, "-SingleWindow"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ) as peer:
        try:
            call(
                "glass_start",
                run=["cmd.exe", "/d", "/c", "exit", "0"],
                window_hint={"title": external_title},
                backend="windows",
                sandbox="off",
                a11y=True,
            )
            unproven = revision(restarted, kind="full", cacheable=False)
            assert "Main window content" in unproven["outline"]
            assert unproven["full_reason"] == "context_unproven"
            call("glass_stop", error=True)
            assert peer.poll() is None, "Glass stopped a fixture it did not launch"
        finally:
            peer.terminate()


def main(server):
    with TemporaryDirectory(prefix="glass-windows-revisions-") as directory:
        root = Path(directory)
        client = Client([server, "--tool-profile", "full"], root / "wire")
        try:
            client.initialize()
            qualify(client, Evidence(root / "evidence", client))
        finally:
            clean = client.close()
        assert clean, "MCP process cleanup failed"
    print("Windows snapshot revision qualification passed")


if __name__ == "__main__":
    main(sys.argv[1])
