#!/usr/bin/env python3
"""Exercise built images through Compose using an isolated loopback-only fake.

Run from a fresh checkout after building Dockerfile.stack and Dockerfile.agent.
No live stack, external credentials, published ports or Docker network allocation.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import stat
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
SECRET = "example-packaging-secret-0123456789abcdef"


def run(args, *, env, check=True):
    result = subprocess.run(args, cwd=ROOT, env=env, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            timeout=300)
    if check and result.returncode:
        raise RuntimeError(f"command failed: {args!r}\n{result.stdout}")
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stack-image", default="pitcrew-stack:local")
    parser.add_argument("--agent-image", default="pitcrew-agent:local")
    parser.add_argument("--docker-socket", default="/var/run/docker.sock",
                        help="Local Docker Unix socket (rootless sockets are supported)")
    args = parser.parse_args()
    socket = Path(args.docker_socket).resolve(strict=True)
    if not stat.S_ISSOCK(socket.stat().st_mode):
        raise SystemExit("--docker-socket must name a local Unix socket")
    docker = ["docker", "--host", f"unix://{socket}"]
    if os.getuid() == 0:
        raise SystemExit("Run as a non-root account to verify private-file permissions.")
    (ROOT / "target").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="packaging-smoke-", dir=ROOT / "target") as directory:
        runtime = Path(directory)
        for child in ["state", "media", "storage"]:
            (runtime / child).mkdir(mode=0o700)
        (runtime / "secret").write_text(SECRET)
        (runtime / "secret").chmod(0o600)
        (runtime / "pitcrew.toml").write_text('''version = 1
database = "/config/state/engine.sqlite"
[engine]
mode = "shadow"
listen = "127.0.0.1:7400"
control_secret = { file = "/config/secret" }
admin_secret = { file = "/config/secret" }
poll_enabled = false
poll_seconds = 1
[stack]
enabled = true
intake_database = "/config/state/intake.sqlite"
web_listen = "127.0.0.1:7401"
web_public_origin = "http://127.0.0.1:7401"
plex_auth_url = "https://example.invalid/"
[runtime]
sync_seconds = 3600
[plex]
url = "http://127.0.0.1:18080/"
owner_token = { file = "/config/secret" }
server_id = "synthetic-packaging-server"
[[arr]]
id = "films"
kind = "radarr"
url = "http://127.0.0.1:18080/"
api_key = { file = "/config/secret" }
libraries = ["1"]
[agent]
url = "http://127.0.0.1:8790"
shared_secret = { file = "/config/secret" }
media_roots = ["/media"]
storage_roots = ["/storage"]
plex_log_timezone = "UTC"
decode_concurrency = 2
[agent.server]
listen = "127.0.0.1:8790"
[admin]
database = "/config/state/admin.sqlite"
engine_database = "/config/state/engine.sqlite"
intake_database = "/config/state/intake.sqlite"
bind = "127.0.0.1:7402"
public_origin = "http://127.0.0.1:7402"
plex_auth_url = "https://example.invalid/"
client_id = "synthetic-packaging-admin"
admin_account_ids = ["1001"]
''')
        (runtime / "pitcrew.toml").chmod(0o600)
        env = dict(os.environ, PITCREW_HOME=directory, PITCREW_UID=str(os.getuid()),
                   PITCREW_GID=str(os.getgid()), PITCREW_IMAGE=args.stack_image,
                   PITCREW_AGENT_IMAGE=args.agent_image,
                   PITCREW_MEDIA=str(runtime / "media"),
                   PITCREW_STORAGE=str(runtime / "storage"),
                   PITCREW_CONTROL_PORT="7400", PITCREW_WEB_PORT="7401",
                   PITCREW_ADMIN_PORT="7402", PITCREW_AGENT_PORT="8790")
        for key in ["DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_TLS", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"]:
            env.pop(key, None)
        compose = [*docker, "compose", "--env-file", "/dev/null", "--project-name",
                   runtime.name, "-f", "packaging/compose.yaml", "-f",
                   "packaging/tests/compose.yaml", "--profile", "admin", "--profile", "agent"]
        def call(*parts):
            return run([*compose, *parts], env=env)
        try:
            config = json.loads(call("config", "--format", "json"))
            services = config["services"]
            assert services["fake"]["network_mode"] == "none"
            for service in ["control", "web", "admin", "agent"]:
                assert services[service]["network_mode"] == "service:fake"
                assert not services[service].get("ports")
                assert services[service]["read_only"]
            call("up", "-d", "--no-build", "--wait", "--wait-timeout", "120")
            call("exec", "-T", "control", "sh", "-ec",
                 'test "$(id -u)" != 0; '
                 'for tool in ffmpeg ffprobe pitcrew-agent rustc; do if command -v "$tool"; then exit 1; fi; done; '
                 'test -x /usr/local/bin/pitcrew-web; test -x /usr/local/bin/pitcrew-bot; '
                 'test -x /usr/local/bin/pitcrew-admin; pitcrew-bot --help; '
                 'pitcrew doctor --config /config/pitcrew.toml')
            call("exec", "-T", "agent", "sh", "-ec",
                 'test "$(id -u)" != 0; ffmpeg -version; ffprobe -version; if command -v rustc; then exit 1; fi')
            # Exercise the authenticated API, in addition to the unauthenticated healthcheck.
            health = json.loads(call("exec", "-T", "agent", "curl", "--noproxy", "*",
                "--fail", "--silent", "--max-time", "5", "-H", f"Authorization: Bearer {SECRET}",
                "http://127.0.0.1:8790/v1/health"))
            assert health
            deadline = time.monotonic() + 30
            while True:
                control_log = call("logs", "--no-log-prefix", "control")
                if (runtime / "state/intake.catalog.json").exists() and '"event":"runtime_tick"' in control_log:
                    break
                if time.monotonic() >= deadline:
                    raise AssertionError(f"catalog sync or runtime tick did not finish: {control_log}")
                time.sleep(0.25)
            assert '"event":"runtime_tick_incomplete"' not in control_log, control_log
            assert '"event":"catalog_sync_incomplete"' not in control_log, control_log
            records = [json.loads(line) for line in call("logs", "--no-log-prefix", "fake").splitlines()]
            requests = [r for r in records if "method" in r]
            assert sorted((r["method"], r["path"], r["status"]) for r in requests) == [
                ("GET", "/api/v3/movie", 200),
                ("GET", "/library/sections", 200),
                ("GET", "/library/sections/1/all", 200)]
            assert not any("501" in r.get("http_log", "") for r in records), records
            for image in [args.stack_image, args.agent_image]:
                info = json.loads(run([*docker, "image", "inspect", image], env=env))[0]
                assert info["Config"]["User"].split(":")[0] not in ["", "0", "root"]
                print(json.dumps({"image": image, "size_bytes": info["Size"],
                                  "default_user": info["Config"]["User"]}))
            print("PASS: control, web, admin and agent healthy; catalog synced; exactly three authenticated GETs; no external network.")
        except BaseException:
            print(run([*compose, "logs", "--no-color", "--tail", "100"], env=env, check=False))
            raise
        finally:
            call("down", "--volumes", "--remove-orphans", "--timeout", "10")


if __name__ == "__main__":
    main()
