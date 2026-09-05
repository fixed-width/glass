"""Read-only loopback serving for the fixture and a second frame origin."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import threading
from urllib.parse import urlencode, urlsplit


class Fixtures:
    def __init__(self, root):
        self.root = Path(root)
        self.assets = {
            name: (self.root / name).read_bytes()
            for name in ("index.html", "frame.html", "fixture.js")
        }
        assets = self.assets

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                name = urlsplit(self.path).path.removeprefix("/")
                if name not in assets:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header(
                    "Content-Type",
                    "text/javascript; charset=utf-8"
                    if name.endswith(".js")
                    else "text/html; charset=utf-8",
                )
                self.send_header("Content-Length", str(len(assets[name])))
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                self.wfile.write(assets[name])

            def log_message(self, *args):
                pass

        self.servers = [
            ThreadingHTTPServer(("127.0.0.1", 0), Handler) for _ in range(2)
        ]
        self.threads = [
            threading.Thread(
                target=s.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True
            )
            for s in self.servers
        ]
        for thread in self.threads:
            thread.start()

    def url(self, case, config):
        query = urlencode(
            {
                "case": case,
                "frame_port": self.servers[1].server_port,
                "delay_ms": config.get("delay_ms", 3000),
                "motion_ms": config.get("motion_ms", 3000),
            }
        )
        return f"http://127.0.0.1:{self.servers[0].server_port}/index.html?{query}"

    def close(self):
        for server in self.servers:
            server.shutdown()
            server.server_close()
        for thread in self.threads:
            thread.join(timeout=1)
