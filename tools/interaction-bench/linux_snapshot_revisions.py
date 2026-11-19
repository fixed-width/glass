"""Exercise MCP snapshot revisions with an owned Linux display and a native GTK app."""

import os
from pathlib import Path
from shutil import which
import sys
from tempfile import TemporaryDirectory
import time

from evidence import Evidence
from protocol import Client
from snapshot_revisions import SnapshotReceiver


REVISION = "glass_a11y_snapshot_diff"
FIXTURE = (
    Path(__file__).resolve().parents[2]
    / "crates/glass-a11y-linux/tests/fixtures/snapshot_revision_fixture.py"
)


def descendants(pid):
    children = set()
    for task in Path(f"/proc/{pid}/task").glob("*/children"):
        try:
            children.update(map(int, task.read_text().split()))
        except FileNotFoundError:
            continue
    return children.union(*(descendants(child) for child in children))


def process_is_alive(pid):
    try:
        state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
    except (FileNotFoundError, ProcessLookupError):
        return False
    return state != "Z"


def qualify(client, evidence, backend):
    receiver = SnapshotReceiver()

    def call(tool, error=False, **arguments):
        reply = client.rpc("tools/call", {"name": tool, "arguments": arguments})
        decoded = evidence.decode(tool, reply)
        assert decoded["is_error"] == error, decoded["texts"]
        return decoded

    def revision(previous=None, *, kind, cacheable=True, max_nodes=None):
        base = previous["revision"] if previous else None
        sequence = client.sequence + 1
        receiver.begin(sequence, base)
        packet = call(REVISION, base_revision=base, max_nodes=max_nodes)
        current = receiver.accept(sequence, packet)
        assert current["kind"] == kind, packet["envelope"]
        assert current["cacheable"] == cacheable, packet["envelope"]
        fresh = call("glass_a11y_snapshot", max_nodes=max_nodes)
        assert fresh["observations"][0] == current["outline"]
        # Compact outlines keep the last preorder ID because the last node is a leaf.
        last_id = int(fresh["observations"][0].splitlines()[-1].split()[0][1:])
        assert last_id + 1 == current["completeness"]["count"]
        return current

    def set_text(field, text):
        call("glass_set_value", target={"query": field}, text=text)

    def start(gdk_backend=backend):
        environment = {"GDK_BACKEND": gdk_backend, "LIBGL_ALWAYS_SOFTWARE": "1"}
        if gdk_backend == "wayland":
            environment["GSK_RENDERER"] = "cairo"
        call(
            "glass_start",
            run=[sys.executable, str(FIXTURE)],
            env=environment,
            window_hint={"title": "Revision main"},
            backend=backend,
            a11y=True,
            sandbox="off",
        )
        end = time.monotonic() + 12
        selected = False
        previous_outline = None
        while time.monotonic() < end:
            reply = client.rpc(
                "tools/call", {"name": "glass_a11y_snapshot", "arguments": {}}
            )
            read = evidence.decode("glass_a11y_snapshot", reply)
            if not read["is_error"] and "Sibling window content" in read["observations"][0]:
                outline = read["observations"][0]
                if selected:
                    if outline == previous_outline:
                        return
                    previous_outline = outline
                else:
                    windows = call("glass_list_windows")["observations"][0]
                    main = next(
                        (window for window in windows if window["title"] == "Revision main"),
                        None,
                    )
                    if main is not None:
                        if not main["active"]:
                            call("glass_select_window", id=main["id"])
                        selected = True
            time.sleep(0.05)
        raise AssertionError("The two-window AT-SPI outline did not settle")

    def stop():
        owned = descendants(client.process.pid)
        assert owned, "Fixture and display processes were not observed"
        call("glass_stop")
        end = time.monotonic() + 2
        while owned and time.monotonic() < end:
            owned = {pid for pid in owned if process_is_alive(pid)}
            if owned:
                time.sleep(0.02)
        assert not owned, f"Owned descendants survived glass_stop: {owned}"

    start()
    baseline = revision(kind="full")
    assert "Revision main" in baseline["outline"]
    assert "Revision sibling" in baseline["outline"]
    assert any(
        client.calls[reference["origin_call"] - 1]["tool"] == REVISION
        for reference in evidence.references
    ), "The full revision should exercise resource follow-ups"
    stable = revision(baseline, kind="unchanged")

    set_text("Revision target", "initial")
    noop = revision(stable, kind="unchanged")
    set_text("Revision decoy", "wrong-field")
    wrong = revision(noop, kind="diff")
    assert 'value="initial"' in wrong["outline"]
    assert 'value="wrong-field"' in wrong["outline"]
    set_text("Revision target", "confirm-é")
    changed = revision(wrong, kind="diff")
    assert 'value="confirm-é"' in changed["outline"]
    assert 'value="initial"' not in changed["outline"]

    call(REVISION, base_revision=changed["revision"])
    recovered = revision(changed, kind="full")
    assert recovered["full_reason"] == "baseline_unavailable"
    receiver = SnapshotReceiver()
    recovered = revision(kind="full")
    assert recovered["full_reason"] == "requested"

    windows = call("glass_list_windows")["observations"][0]
    sibling = next(window["id"] for window in windows if window["title"] == "Revision sibling")
    call("glass_select_window", id=sibling)
    switched = revision(recovered, kind="full")
    assert switched["context_id"] != recovered["context_id"]
    call("glass_select_window", id=sibling)
    reselected = revision(switched, kind="full")
    assert reselected["context_id"] != switched["context_id"]
    resized = call("glass_window", op="resize", width=460, height=320)
    assert resized["envelope"]["result"]["width"] == 460
    geometry = revision(reselected, kind="full")
    assert geometry["context_id"] != reselected["context_id"]
    limited = revision(geometry, kind="full", max_nodes=500)
    assert limited["context_id"] != geometry["context_id"]
    partial = revision(limited, kind="full", cacheable=False, max_nodes=1)
    assert partial["completeness"]["truncated"]["limit"] == "nodes"

    stop()
    call(REVISION, error=True, base_revision=limited["revision"])
    start()
    restarted = revision(limited, kind="full")
    assert restarted["context_id"] != limited["context_id"]

    set_text("Revision decoy", "launch-peer")
    ready = call(
        "glass_wait_for_log", contains="REVISION_PEER_REGISTERED",
        stream="stdout", cursor=0, timeout_ms=12_000,
    )
    assert ready["envelope"]["result"]["matched"], "Peer did not register on the AT-SPI bus"
    unproven = revision(restarted, kind="full", cacheable=False)
    assert unproven["full_reason"] == "context_unproven"
    stop()

    if backend == "wayland":
        if which("Xwayland"):
            start(gdk_backend="x11")
            xwayland = call(REVISION)["envelope"]["result"]
            assert xwayland["kind"] == "full"
            assert xwayland["cacheable"] is False
            assert xwayland["full_reason"] == "context_unproven"
            stop()
        else:
            print("Xwayland executable unavailable. Skipping the Xwayland revision case.")


def main(server, backend):
    with TemporaryDirectory(prefix=f"glass-{backend}-revisions-") as root:
        root = Path(root)
        environment = os.environ.copy()
        for key in (
            "GLASS_DISPLAY",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "AT_SPI_BUS_ADDRESS",
        ):
            environment.pop(key, None)
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        environment["XDG_RUNTIME_DIR"] = str(runtime)
        client = Client([server, "--tool-profile", "full"], root / "wire", env=environment)
        try:
            client.initialize()
            qualify(client, Evidence(root / "evidence", client), backend)
        finally:
            clean = client.close()
        assert clean, "MCP process cleanup failed"
    print(f"{backend} snapshot revision qualification passed")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
