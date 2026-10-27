"""Isolation regressions run without Flask or the private reference."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ADAPTER = Path(__file__).with_name('baseline.py')


class Isolation(unittest.TestCase):
    def test_external_adapter_observes_each_conversation_in_fresh_private_state(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            reference, scratch = root / 'reference', root / 'scratch'
            reference.mkdir()
            scratch.mkdir()
            source = '''from pathlib import Path
def observe(requests):
    assert not Path('state').exists()
    Path('state').write_text('owned')
    return [{'case_id': row['case_id'], 'turn': row['turn'], 'reply': row['body']['text']} for row in requests]
'''
            (reference / 'adapter.py').write_text(source)
            requests = [{'case_id': case, 'turn': turn, 'body': {'text': f'{case}-{turn}'}}
                        for case, turn in [('a', 0), ('a', 1), ('b', 0)]]
            result = subprocess.run([sys.executable, '-I', '-B', str(ADAPTER), str(reference), str(scratch)],
                input=json.dumps(requests), capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), [
                {'case_id': row['case_id'], 'turn': row['turn'], 'reply': row['body']['text']}
                for row in requests])
            self.assertEqual((reference / 'adapter.py').read_text(), source)
            self.assertEqual(sorted(path.name for path in reference.iterdir()), ['adapter.py'])
            self.assertTrue((scratch / 'case-0' / 'state').is_file())
            self.assertTrue((scratch / 'case-1' / 'state').is_file())

    def test_missing_callback_and_forbidden_external_adapter_fail(self):
        for source in ['pass', 'def observe(requests):\n import socket\n socket.socket()']:
            with self.subTest(source=source), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                reference, scratch = root / 'reference', root / 'scratch'
                reference.mkdir()
                scratch.mkdir()
                (reference / 'adapter.py').write_text(source)
                result = subprocess.run([sys.executable, '-I', '-B', str(ADAPTER), str(reference), str(scratch)],
                    input='[{"case_id":"a","turn":0}]', capture_output=True, text=True, timeout=10)
                self.assertNotEqual(result.returncode, 0)

    def check_child(self, operation):
        with tempfile.TemporaryDirectory() as directory:
            script = '''
import importlib.util, pathlib, socket, sqlite3, subprocess, os, sys
spec = importlib.util.spec_from_file_location('baseline', sys.argv[1])
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
root = pathlib.Path(sys.argv[2])
violations = m.isolate(root)
try:
    exec(sys.argv[3])
except PermissionError:
    pass
else:
    raise SystemExit('forbidden operation succeeded')
assert violations
'''
            result = subprocess.run([sys.executable, '-I', '-B', '-c', script,
                str(ADAPTER), directory, operation], capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_socket_creation_denied_before_a_connection_can_exist(self):
        self.check_child('socket.socket()')

    def test_dns_and_subprocess_denied(self):
        self.check_child("socket.getaddrinfo('drill.invalid', 80)")
        self.check_child("subprocess.run([sys.executable, '-c', 'pass'])")

    def test_writes_and_sqlite_outside_owned_root_denied(self):
        self.check_child("(root.parent / 'forbidden-baseline-write').write_text('no')")
        self.check_child("sqlite3.connect(str(root.parent / 'forbidden-baseline.sqlite'))")
        self.check_child("os.mkdir(root.parent / 'forbidden-baseline-directory')")

    def test_readonly_reference_and_private_scratch_work(self):
        with tempfile.TemporaryDirectory() as directory:
            code = '''
import importlib.util, pathlib, sqlite3, sys
spec = importlib.util.spec_from_file_location('baseline', sys.argv[1])
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
root = pathlib.Path(sys.argv[2])
violations = m.isolate(root)
assert pathlib.Path(sys.argv[1]).read_text()
(root / 'allowed').write_text('fixture')
connection = sqlite3.connect(str(root / 'owned.sqlite'))
connection.execute('create table fixture (value integer)')
connection.close()
assert not violations
'''
            result = subprocess.run([sys.executable, '-I', '-B', '-c', code,
                str(ADAPTER), directory], capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == '__main__':
    unittest.main()
