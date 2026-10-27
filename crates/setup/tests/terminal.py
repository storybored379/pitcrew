"""Exercise the real prompt in an isolated pseudo-terminal; no network or services."""
import os
import pty
import select
import subprocess
import sys
import termios
import time


def run(mode):
    master, slave = pty.openpty()
    original = termios.tcgetattr(slave)
    original[3] |= termios.ECHO | termios.ECHONL | termios.ISIG | termios.ICANON
    original[6][termios.VMIN] = 7
    original[6][termios.VTIME] = 4
    termios.tcsetattr(slave, termios.TCSANOW, original)
    original = termios.tcgetattr(slave)
    process = subprocess.Popen(
        [sys.argv[1], "--exact", "terminal_child", "--nocapture"],
        stdin=slave, stdout=slave, stderr=slave,
        env={**os.environ, "PITCREW_SETUP_TEST_PROMPT": mode},
    )
    transcript = b""
    try:
        deadline = time.monotonic() + 5
        while b"Setup input: " not in transcript:
            assert time.monotonic() < deadline, "prompt did not appear"
            if select.select([master], [], [], 0.05)[0]:
                transcript += os.read(master, 65536)
            assert process.poll() is None, transcript.decode(errors="replace")
        hidden = termios.tcgetattr(slave)
        if mode in ("secret", "cancel"):
            assert not hidden[3] & (termios.ECHO | termios.ECHONL | termios.ISIG | termios.ICANON)
            assert hidden[6][termios.VMIN] == 1
            assert hidden[6][termios.VTIME] == 0
        else:
            assert hidden == original
        inputs = {
            "secret": b"synthetic-secretX\x7f\n",
            "cancel": b"synthetic-secret\x03",
            "plain": b"plain value \n",
            "eof": b"\x04",
        }
        os.write(master, inputs[mode])
        assert process.wait(timeout=5) == 0
        while select.select([master], [], [], 0.05)[0]:
            transcript += os.read(master, 65536)
        assert b"synthetic-secret" not in transcript
        assert termios.tcgetattr(slave) == original, "terminal settings were not restored"
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(slave)
        os.close(master)


for mode in ("secret", "cancel", "plain", "eof"):
    run(mode)
pipe = subprocess.run(
    [sys.argv[1], "--exact", "terminal_child", "--nocapture"],
    input=b"synthetic-secret\n", capture_output=True,
    env={**os.environ, "PITCREW_SETUP_TEST_PROMPT": "pipe"}, timeout=5,
)
assert pipe.returncode == 0, pipe.stderr
assert b"synthetic-secret" not in pipe.stdout + pipe.stderr
