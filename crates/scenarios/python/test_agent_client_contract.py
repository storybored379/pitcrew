"""Prove optional agent-client extraction does not execute reference globals."""
from pathlib import Path
import tempfile
import unittest
from agent_client_contract import client_type


class ClientIsolationTests(unittest.TestCase):
    def test_only_selected_class_executes_and_reference_stays_unchanged(self):
        source = '''raise RuntimeError("reference global executed")
class Unrelated:
    raise RuntimeError("unrelated class executed")
class ExternalClient:
    def __init__(self, url):
        self.url = url
    def health(self):
        return http_json("GET", self.url + "/health")
'''
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'reference.py'
            path.write_text(source)
            calls = []
            def transport(method, url):
                calls.append((method, url))
                return {'fixture': True}
            client = client_type(path, transport)('http://test.invalid')
            self.assertEqual(client.health(), {'fixture': True})
            self.assertEqual(calls, [('GET', 'http://test.invalid/health')])
            self.assertEqual(path.read_text(), source)

    def test_missing_or_duplicate_client_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'reference.py'
            for source in ('pass', 'class ExternalClient: pass\nclass ExternalClient: pass'):
                path.write_text(source)
                with self.assertRaises(ValueError):
                    client_type(path, None)
