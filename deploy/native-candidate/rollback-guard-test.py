from pathlib import Path
import fcntl
import subprocess
import sys
import tempfile
import unittest

GUARD = Path(__file__).with_name("rollback-guard.py")


class RollbackGuardTest(unittest.TestCase):
    def test_capture_check_and_exact_refusal_controls(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            native = root / "native"
            legacy = root / "legacy"
            native.mkdir()
            legacy.mkdir()
            lock = native / "LOCK"
            lock.touch()
            current = native / "CURRENT"
            current.write_bytes(b"before")
            legacy.joinpath("frontier.md").write_bytes(b"retained")
            receipt = root / "certificate.json"

            def guard(operation):
                command = [sys.executable, GUARD, operation, native, legacy]
                if operation == "check":
                    command.extend(["--receipt", receipt])
                return subprocess.run(command, capture_output=True, text=True)

            captured = guard("capture")
            self.assertEqual(captured.returncode, 0, captured.stderr)
            receipt.write_text(captured.stdout)
            self.assertEqual(guard("check").returncode, 0)

            with lock.open() as holder:
                fcntl.flock(holder, fcntl.LOCK_EX | fcntl.LOCK_NB)
                held = guard("check")
                self.assertEqual(held.returncode, 1)
                self.assertIn("lock holder", held.stderr)

            current.write_bytes(b"after a native write")
            changed = guard("check")
            self.assertEqual(changed.returncode, 1)
            self.assertIn("authorities differ", changed.stderr)
            current.write_bytes(b"before")

            extra = legacy / "new authority data"
            extra.write_bytes(b"unexpected")
            self.assertEqual(guard("check").returncode, 1)
            self.assertEqual(extra.read_bytes(), b"unexpected")


if __name__ == "__main__":
    unittest.main()
