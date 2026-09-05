import os
from pathlib import Path
import sys
import tempfile
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from sessions import Session


@unittest.skipUnless(
    sys.platform.startswith("linux"), "Xvfb display ownership is Linux-only"
)
class SessionTests(unittest.TestCase):
    def test_display_pipe_stays_open_until_server_shutdown(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake = root / "Xvfb"
            fake.write_text(f"#!{sys.executable}\n" + """import os, sys, time
fd = int(sys.argv[sys.argv.index('-displayfd') + 1])
os.write(fd, b'99\\n')
time.sleep(0.05)
os.write(fd, b'99\\n')
time.sleep(10)
""")
            fake.chmod(0o700)
            session = Session.__new__(Session)
            session.env = {**os.environ, "PATH": str(root)}
            try:
                self.assertEqual(session.start_display(root / "display.log"), ":99")
                time.sleep(0.15)
                self.assertIsNone(
                    session.display.poll(), "server lost its display pipe"
                )
            finally:
                session.display.terminate()
                session.display.wait(timeout=1)
                session.display_log.close()
                os.close(session.display_fd)


if __name__ == "__main__":
    unittest.main()
