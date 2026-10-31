"""Synthetic Plex/arr reads for the image smoke test. No upstream access."""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit


class Fake(BaseHTTPRequestHandler):
    def do_GET(self):
        path = urlsplit(self.path).path
        bodies = {
            "/library/sections": {"MediaContainer": {"size": 1, "Directory": [
                {"key": "1", "type": "movie", "title": "Synthetic library"}
            ]}},
            "/library/sections/1/all": {
                "MediaContainer": {"size": 0, "totalSize": 0, "offset": 0, "Metadata": []}
            },
            "/api/v3/movie": [],
        }
        expected = "example-packaging-secret-0123456789abcdef"
        authorized = (self.headers.get("X-Plex-Token") == expected if path.startswith("/library/")
                      else self.headers.get("X-Api-Key") == expected)
        status = 200 if authorized and path in bodies else 403 if not authorized else 404
        data = json.dumps(bodies.get(path, {})).encode()
        print(json.dumps({"method": "GET", "path": path, "status": status}), flush=True)
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):
        # Unexpected methods are caught in the transcript without recording secrets.
        print(json.dumps({"http_log": fmt % args}), flush=True)


ThreadingHTTPServer(("127.0.0.1", 18080), Fake).serve_forever()
