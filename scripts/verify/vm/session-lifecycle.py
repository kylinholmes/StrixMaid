#!/usr/bin/env python3
"""Verify real PAM elevation/session expiry in a dedicated Linux systemd VM.

Requires installed strixmaid/helper and PAM configuration, an existing disposable
non-root account, and its private password file. Run as root with
STRIXMAID_TEST_VM=1. Creates only a temporary server/config/database; does not
change the account's groups or password. Defaults to supported short timeouts
(60s session / 30s elevation); pass 900 / 300 to verify production defaults.
"""
import argparse
import grp
import hashlib
import json
import os
from pathlib import Path
import pwd
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def run(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.PIPE, timeout=30)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def wait_for(check, timeout, description):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            print("PASS " + description, flush=True)
            return
        time.sleep(0.5)
    raise RuntimeError("timed out: " + description)


class Client:
    def __init__(self, port):
        self.base = f"http://127.0.0.1:{port}/api/v1"
        self.token = None

    def request(self, method, path, data=None, expected=200):
        headers = {"Content-Type": "application/json"}
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        req = urllib.request.Request(
            self.base + path, method=method, headers=headers,
            data=json.dumps(data).encode() if data is not None else None,
        )
        try:
            response = urllib.request.urlopen(req, timeout=20)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            body = response.read()
            require(response.status == expected,
                    f"{method} {path}: expected {expected}, got {response.status}")
            return json.loads(body) if body else None

    def authenticate(self, username, password, elevate=False):
        prefix = "/auth/elevate" if elevate else "/auth"
        response = self.request("POST", prefix + "/start", {"username": username})
        for _ in range(5):
            response = self.request("POST", prefix + "/respond", {
                "session": response["session"],
                "responses": [
                    {"id": p["id"], "value": password if p["style"] == "prompt" else ""}
                    for p in response.get("prompts", [])
                ],
            })
            if response.get("status") == "complete":
                if not elevate:
                    self.token = response["token"]
                return
            require(response.get("status") == "more", "PAM authentication failed")
        raise RuntimeError("PAM conversation exceeded five rounds")


def descendants(root_pid):
    rows = {}
    for line in run("ps", "-eo", "pid=,ppid=,uid=,comm=").splitlines():
        pid, parent, uid, comm = line.split(maxsplit=3)
        rows[int(pid)] = (int(parent), int(uid), Path(comm).name)
    found = {root_pid}
    while True:
        added = {pid for pid, (parent, _, _) in rows.items() if parent in found} - found
        if not added:
            return {pid: rows[pid] for pid in found if pid != root_pid}
        found.update(added)


def check_lifecycle(client, db, server_pid, user, password, idle, elevated):
    uid = pwd.getpwnam(user).pw_uid

    def workers():
        return {pid: row[1] for pid, row in descendants(server_pid).items()
                if row[2] == "strixmaid"}

    def query(sql, args=()):
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
            return conn.execute(sql, args).fetchall()

    client.authenticate(user, password)
    session = client.request("GET", "/auth/session")
    require(session["session_opened"] and not session["elevated"], "PAM user session not open")
    wait_for(lambda: list(workers().values()) == [uid], 10, "one user worker after PAM login")
    user_workers = set(workers())
    own_term = client.request("POST", "/terminals", {}, expected=201)["id"]
    terminals = client.request("GET", "/terminals")
    own_shell = next(t["pid"] for t in terminals if t["id"] == own_term)
    require(next(t["uid"] for t in terminals if t["id"] == own_term) == uid,
            "own terminal has wrong uid")

    client.authenticate(user, password, elevate=True)
    require(client.request("GET", "/auth/session")["elevated"], "elevation not reported")
    wait_for(lambda: sorted(workers().values()) == [0, uid], 10, "user and admin workers after elevation")
    admin_workers = set(workers()) - user_workers
    root_term = client.request("POST", "/terminals", {"user": "root"}, expected=201)["id"]
    require(any(t["id"] == root_term and t["uid"] == 0
                for t in client.request("GET", "/terminals")), "admin terminal has wrong uid")
    client.request("DELETE", "/terminals/" + root_term, expected=204)
    token_hash = hashlib.sha256(client.token.encode()).hexdigest()
    require(query("SELECT id FROM sessions") == [(token_hash,)], "session must store only token hash")
    require(query("SELECT elevated FROM node_sessions") == [(1,)], "elevated flag not persisted")

    # Session reads keep the user session alive, but must not renew admin access.
    deadline = time.monotonic() + elevated + 15
    while any(Path(f"/proc/{pid}").exists() for pid in admin_workers):
        require(time.monotonic() < deadline, "admin worker was not reaped after idle timeout")
        client.request("GET", "/auth/session")
        time.sleep(1)
    session = client.request("GET", "/auth/session")
    require(not session["elevated"] and session.get("elevated_ts") is None, "stale elevated state")
    require(set(workers()) == user_workers, "user worker was lost during demotion")
    wait_for(lambda: sum(row[2].startswith("strixmaid-helpe")
                         for row in descendants(server_pid).values()) == 1,
             10, "admin helper reaped; user helper survives")
    require(query("SELECT elevated FROM node_sessions") == [(0,)], "demotion not persisted")
    denied = client.request("POST", "/terminals", {"user": "root"}, expected=403)
    require(denied["code"] == "elevation_required", "expired admin operation was not denied")
    require(any(t["id"] == own_term for t in client.request("GET", "/terminals")),
            "user terminal disappeared during demotion")
    print("PASS elevation expires despite ordinary activity; root denied, user terminal survives", flush=True)

    # No authenticated polling here: that would keep the session alive forever.
    remaining = set(descendants(server_pid)) | {own_shell}
    print(f"waiting up to {idle + 20}s without authenticated requests", flush=True)
    wait_for(lambda: not any(Path(f"/proc/{pid}").exists() for pid in remaining),
             idle + 20, "idle session reaps helpers, workers and terminal shell")
    client.request("GET", "/auth/session", expected=401)
    require(query("SELECT id FROM sessions") == [], "expired session row remains")
    require(query("SELECT session_id FROM node_sessions") == [], "expired node session row remains")
    for action in ("session.drop_elevation", "session.expire"):
        require(query("SELECT username, target, result FROM audit_log WHERE action = ?", (action,))
                == [("[system]", user, "ok")], f"missing or duplicated audit: {action}")
    print("PASS old token rejected, database cleared, timeout audits recorded once", flush=True)
    return client.token


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("user")
    parser.add_argument("password_file", type=Path)
    parser.add_argument("--idle-timeout", type=int, default=60)
    parser.add_argument("--elevated-timeout", type=int, default=30)
    args = parser.parse_args()
    require(os.geteuid() == 0 and os.environ.get("STRIXMAID_TEST_VM") == "1",
            "run as root in the dedicated VM with STRIXMAID_TEST_VM=1")
    require(args.idle_timeout >= 60 and args.elevated_timeout >= 30,
            "timeouts must satisfy product minimums (60 / 30)")
    account = pwd.getpwnam(args.user)
    require(account.pw_uid != 0, "use a disposable non-root test account")
    require(args.password_file.is_file() and args.password_file.stat().st_mode & 0o077 == 0,
            "password file must be private")
    password = args.password_file.read_text().strip()
    require(bool(password), "empty password file")
    # No changes to the host account: allow its primary group only in this server.
    group = grp.getgrgid(account.pw_gid).gr_name
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    unit = f"strixmaid-session-check-{os.getpid()}.service"
    with tempfile.TemporaryDirectory(prefix="strixmaid-session-check-") as scratch:
        root = Path(scratch)
        config = root / "config.toml"
        config.write_text(
            f'listen = "127.0.0.1:{port}"\ndata_dir = {json.dumps(str(root / "data"))}\n'
            'helper_path = "/usr/local/bin/strixmaid-helper"\n'
            f'[session]\nelevate_groups = [{json.dumps(group)}]\n'
            f'idle_timeout_secs = {args.idle_timeout}\n'
            f'elevated_idle_timeout_secs = {args.elevated_timeout}\n'
        )
        run("/usr/local/bin/strixmaid", "--config", str(config), "--check-config", "serve")
        try:
            run("systemd-run", "--unit=" + unit, "--service-type=exec", "--collect",
                "--setenv=RUST_LOG=trace", "/usr/local/bin/strixmaid", "--config", str(config), "serve")
            server_pid = int(run("systemctl", "show", unit, "-p", "MainPID", "--value"))
            require(server_pid > 1, "server not started")
            client = Client(port)

            def healthy():
                try:
                    return client.request("GET", "/health")["status"] == "ok"
                except (OSError, RuntimeError):
                    return False

            wait_for(healthy, 30, "isolated server healthy")
            token = check_lifecycle(client, root / "data/strixmaid.db", server_pid, args.user,
                                    password, args.idle_timeout, min(args.idle_timeout, args.elevated_timeout))
            logs = run("journalctl", "-u", unit, "--no-pager", "-o", "cat")
            require(password not in logs and token not in logs, "credentials found in trace logs")
            for path in (root / "data").glob("strixmaid.db*"):
                data = path.read_bytes()
                require(password.encode() not in data and token.encode() not in data,
                        "plaintext credentials found in database or WAL")
            print("PASS password/token absent from trace logs and database/WAL", flush=True)
        finally:
            run("systemctl", "stop", unit)
            print("CLEANUP temporary server stopped; temporary config/database removed on exit", flush=True)


if __name__ == "__main__":
    main()
