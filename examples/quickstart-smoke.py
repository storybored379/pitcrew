#!/usr/bin/env python3
"""Exercise init + doctor + actual shadow/viewer binaries with owned loopback fakes.

Requires Python 3 and OpenSSL. Run after `cargo build --locked -p pitcrew`:
    python3 examples/quickstart-smoke.py target/debug/pitcrew

No Docker daemon, real Plex login, media files, or upstream credentials are used.
The fake catalog below exercises intake; it does not claim automatic catalog sync
or a certified repair. All temporary files and owned processes are cleaned up.
"""
import contextlib
import http.cookiejar
import http.server
import json
import os
from pathlib import Path
import re
import socket
import ssl
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request


class Upstream(http.server.BaseHTTPRequestHandler):
    calls = []

    def log_message(self, *_):
        pass

    def do_GET(self):
        self.reply()

    def do_POST(self):
        self.reply()

    def do_PUT(self):
        self.reply()

    def do_PATCH(self):
        self.reply()

    def do_DELETE(self):
        self.reply()

    def reply(self):
        path = urllib.parse.urlsplit(self.path).path
        self.calls.append((self.command, path))
        routes = {
            ("GET", "/library/sections"): {
                "MediaContainer": {"size": 1, "Directory": [{"key": "1", "type": "movie", "title": "Films"}]}
            },
            ("GET", "/api/v3/system/status"): {"version": "6.3.0.10514"},
            ("POST", "/api/v2/pins"): {"id": 1, "code": "synthetic-pin"},
            ("GET", "/api/v2/pins/1"): {"authToken": "synthetic-viewer"},
            ("GET", "/api/v2/user"): {"id": 1},
            ("GET", "/api/v2/resources"): [{"clientIdentifier": "example-server", "provides": "server"}],
        }
        data = routes.get((self.command, path))
        if path == "/api/v3/system/status" and not self.headers.get("X-Api-Key"):
            data = None
        if path.startswith("/library/") and not self.headers.get("X-Plex-Token"):
            data = None
        self.send_response(200 if data is not None else 404)
        self.send_header("Content-Type", "application/json")
        body = json.dumps(data).encode()
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def run(binary, *args):
    result = subprocess.run([str(binary), *map(str, args)], capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise RuntimeError(f"command failed: {args[0]}; output intentionally withheld")
    return result.stdout


def wait_for(client, url, process):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError("owned stack process exited before becoming ready")
        try:
            with client.open(url, timeout=1) as response:
                if response.status == 200:
                    return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("owned stack process did not become ready")


@contextlib.contextmanager
def child(binary, args, log):
    with log.open("w+") as output:
        process = subprocess.Popen([str(binary), *map(str, args)], stdout=output, stderr=output)
        try:
            yield process
        finally:
            if process.poll() not in (None, 0):
                output.seek(0)
                sys.stderr.write(output.read())
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=10)


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: python3 examples/quickstart-smoke.py PATH_TO_PITCREW")
    binary = Path(sys.argv[1]).resolve(strict=True)
    # Avoid sending even synthetic credentials through an ambient proxy.
    for name in list(os.environ):
        if name.lower().endswith("_proxy"):
            os.environ.pop(name)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    thread = None
    try:
        with tempfile.TemporaryDirectory(prefix="pitcrew-smoke-") as directory:
            root = Path(directory)
            # A local trust anchor exercises the HTTPS-only Plex auth contract.
            # Trust is scoped to our child processes; no system trust store changes.
            def openssl(*args):
                subprocess.run(["openssl", *map(str, args)], check=True, capture_output=True, timeout=20)
            ca, ca_key = root / "ca.pem", root / "ca.key"
            cert, key, csr = root / "server.pem", root / "server.key", root / "server.csr"
            openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-keyout", ca_key, "-out", ca, "-subj", "/CN=Pit Crew smoke CA")
            openssl("req", "-newkey", "rsa:2048", "-nodes", "-keyout", key,
                    "-out", csr, "-subj", "/CN=localhost")
            extensions = root / "extensions.cnf"
            extensions.write_text("subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\n")
            openssl("x509", "-req", "-in", csr, "-CA", ca, "-CAkey", ca_key,
                    "-CAcreateserial", "-out", cert, "-days", "1", "-extfile", extensions)
            os.environ["SSL_CERT_FILE"] = str(ca)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(cert, key)
            server.socket = context.wrap_socket(server.socket, server_side=True)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            endpoint = f"https://127.0.0.1:{server.server_port}"
            plex = root / "plex-input"
            arr = root / "arr-input"
            for path in (plex, arr):
                path.write_text("synthetic-test-credential")
                path.chmod(0o600)
            output = root / "runtime"
            common = ["--non-interactive", "--plex-url", endpoint, "--plex-server-id", "example-server",
                      "--plex-token-file", plex, "--arr-kind", "radarr", "--arr-url", endpoint,
                      "--arr-api-key-file", arr, "--library", "1"]
            report = run(binary, "init", "--output", output, *common)
            assert "Doctor passed" in report
            assert not Upstream.calls, "offline init contacted an upstream"
            for path in [output / "pitcrew.toml", *(output / "secrets").iterdir()]:
                assert path.stat().st_mode & 0o777 == 0o600
            print("PASS init: private config/secrets, offline doctor, zero upstream requests")

            config = output / "pitcrew.toml"
            run(binary, "doctor", "--config", config, "--connectivity")
            assert set(Upstream.calls) == {("GET", "/library/sections"), ("GET", "/api/v3/system/status")}
            print("PASS doctor --connectivity: authenticated read endpoints on owned fakes only")

            # Use the normal static-catalog seam for this standalone intake example.
            # The production host must supply independently authorized sync evidence.
            control_port, web_port = free_port(), free_port()
            while control_port == web_port:
                web_port = free_port()
            origin = f"http://127.0.0.1:{web_port}"
            source = config.read_text().replace("127.0.0.1:7400", f"127.0.0.1:{control_port}")
            source = source.replace("127.0.0.1:7401", f"127.0.0.1:{web_port}")
            source = re.sub(r"^catalog_snapshot_file = .*\n", "", source, flags=re.MULTILINE)
            source = source.replace("[stack]\n", f'[stack]\nplex_auth_url = "{endpoint}/"\n')
            source += '\n[[stack.catalog]]\nid="synthetic-film"\ntitle="Synthetic Film"\nlibrary_id="1"\nlibrary_name="Films"\nresolution="hd"\n'
            config.write_text(source)
            client = urllib.request.build_opener(urllib.request.ProxyHandler({}),
                                                urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))

            def request(path, form=None):
                data = None if form is None else urllib.parse.urlencode(form).encode()
                req = urllib.request.Request(origin + path, data=data, headers={"Origin": origin})
                with client.open(req, timeout=10) as response:
                    return response.read().decode()

            with child(binary, ["--config", config], root / "control.log") as control:
                wait_for(client, f"http://127.0.0.1:{control_port}/healthz", control)
                with child(binary, ["web", "--config", config], root / "web.log") as web:
                    wait_for(client, origin + "/", web)
                    assert "Sign in with Plex" in request("/")
                    request("/auth/start", {})
                    request("/auth/poll", {})
                    wizard = request("/report?item=synthetic-film&issue=playback")
                    csrf = re.search(r'name="csrf" value="([^"]+)"', wizard).group(1)
                    receipt = request("/report", {"csrf": csrf, "item": "synthetic-film", "nonce": "smoke-report",
                                                   "issue": "playback", "answer": "just_this_one", "text": "This film will not play"})
                    assert "Report received" in receipt
                    assert "Synthetic Film" in request("/my-reports")
                    with sqlite3.connect(output / "state/intake.sqlite") as db:
                        assert db.execute("SELECT count(*) FROM web_intakes").fetchone()[0] == 1
                    print("PASS control + web: health, fake sign-in, report submission, durable My reports")
            assert '"effects_enabled":false' in (root / "control.log").read_text()
            assert all(method == "GET" or path == "/api/v2/pins" for method, path in Upstream.calls)
            print("PASS shadow: no upstream repair writes; no decode/repair certification claimed")
    finally:
        if thread is not None:
            server.shutdown()
            thread.join(timeout=5)
        server.server_close()


if __name__ == "__main__":
    main()
