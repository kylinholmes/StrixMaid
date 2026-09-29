#!/usr/bin/env python3
"""A4 live file-access acceptance with real PAM/workers in a dedicated Linux VM.

Run as root with STRIXMAID_TEST_VM=1, an existing non-root test account and its
mode-0600 password file (same positional arguments as session-lifecycle.py).
Uses installed binaries by default; --server/--helper may select a ready build.
Only creates an independent transient service and files under VM /tmp. Does not
install binaries, change accounts, stop other services, or manage the VM.

Nine logins of the SAME account intentionally test distinct session ownership;
this does not claim coverage of two different Unix accounts. Slow HTTP clients
hold 4/session and 32/node streams without downloading the entire sparse file.
Cold RSS retention is reported; later rounds use the first 60-second idle RSS as
their warm baseline. RSS is sampled, not an exact allocator-reclamation measure.
No expiry-time travel: the 600-second natural expiry is not exercised here.
"""

import argparse
from contextlib import contextmanager
import hashlib
import http.client
from http.cookies import SimpleCookie
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from urllib.parse import quote, urlsplit

# Reuse the established PAM conversation, process inspection and service helpers
# without creating __pycache__ in the shared source tree.
sys.dont_write_bytecode = True
_spec = importlib.util.spec_from_file_location(
    "session_lifecycle", Path(__file__).with_name("session-lifecycle.py"))
lifecycle = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(lifecycle)
require = lifecycle.require
run = lifecycle.run
wait_for = lifecycle.wait_for

MIB = 1024 * 1024
DENIED = (401, 403, 404)
SECRETS = set()
DIAGNOSTIC_RELEASE = threading.Event()


def passed(message):
    print("PASS " + message, flush=True)


def metric(label, **values):
    print("METRIC " + label + " " + json.dumps(values, sort_keys=True), flush=True)


class Transfer:
    def __init__(self, connection, response):
        self.connection = connection
        self.response = response
        self.status = response.status
        self.headers = response.headers

    def close(self):
        # Closing the TCP socket actively cancels, including unread HTTP bodies.
        sock = self.connection.sock
        if sock is not None:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        self.response.close()
        self.connection.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class Client(lifecycle.Client):
    def __init__(self, port, timeout=5):
        super().__init__(port)
        self.port = port
        self.timeout = timeout
        self.cookie = None
        self.cookie_name = None
        self.last_headers = None

    def open(self, method, path, data=None, *, bearer=True, cookie=True,
             headers=None, slow=False):
        parsed = urlsplit(path)
        require(not parsed.scheme and not parsed.netloc and not parsed.fragment,
                "API URL must be local")
        target = path if path.startswith("/api/v1/") else "/api/v1" + path
        h = {"Accept-Encoding": "identity", "Connection": "close"}
        if bearer and self.token:
            h["Authorization"] = "Bearer " + self.token
        if cookie and self.cookie:
            h["Cookie"] = self.cookie
        if data is not None:
            h["Content-Type"] = "application/json"
        h.update(headers or {})
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=self.timeout)
        try:
            conn.connect()
            if slow:
                conn.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 16384)
            conn.request(method, target, body=json.dumps(data).encode()
                         if data is not None else None, headers=h)
            return Transfer(conn, conn.getresponse())
        except BaseException:
            conn.close()
            raise

    def fetch(self, method, path, data=None, expected=200, limit=8*MIB, **kwargs):
        with self.open(method, path, data, **kwargs) as transfer:
            choices = (expected,) if isinstance(expected, int) else expected
            require(transfer.status in choices,
                    f"{method}: expected status {choices}, got {transfer.status}")
            if method == "HEAD":
                # HTTPResponse.read() always ignores a HEAD body. Check the wire
                # after headers instead, with Connection: close and a timeout.
                require(transfer.response.fp.read(1) == b"", "HEAD sent body bytes")
                body = b""
            else:
                body = transfer.response.read(limit + 1)
                require(len(body) <= limit, "response exceeded bounded read limit")
            self.last_headers = transfer.headers
            return body, transfer.headers

    def request(self, method, path, data=None, expected=200, **kwargs):
        body, _ = self.fetch(method, path, data, expected=expected, **kwargs)
        return json.loads(body) if body else None

    def authenticate(self, username, password, elevate=False):
        super().authenticate(username, password, elevate)
        SECRETS.add(self.token)

    def accept_cookie(self, headers):
        raw = headers.get_all("Set-Cookie") or []
        require(len(raw) == 1, "expected exactly one file Set-Cookie")
        jar = SimpleCookie()
        jar.load(raw[0])
        require(len(jar) == 1, "invalid file cookie")
        name, morsel = next(iter(jar.items()))
        require(morsel["httponly"] and morsel["samesite"].lower() == "strict"
                and morsel["path"] == "/api/v1/file-access" and not morsel["domain"],
                "file cookie attributes are not restricted")
        require(morsel["max-age"].isdigit() and 0 < int(morsel["max-age"]) <= 600,
                "file cookie is not short lived")
        require(not morsel["secure"], "loopback HTTP cookie incorrectly requires HTTPS")
        require(len(morsel.value) >= 32 and morsel.value != self.token,
                "file cookie is not an independent secret")
        SECRETS.add(morsel.value)
        self.cookie_name = name
        self.cookie = f"{name}={morsel.value}"

    def create(self, path, purpose="download"):
        result = self.request("POST", "/file-access", {"path": str(path), "purpose": purpose})
        self.accept_cookie(self.last_headers)
        require(isinstance(result.get("id"), str) and bool(result["id"]), "missing access id")
        require(result.get("url") == "/api/v1/file-access/" + result["id"] + "/content",
                "unexpected file content URL")
        require(type(result.get("expires_in_secs")) is int
                and 0 < result["expires_in_secs"] <= 600, "invalid access lifetime")
        require(all(secret not in result["url"] for secret in SECRETS),
                "credential found in access URL")
        return result

    def content(self, entry, method="GET", **kwargs):
        return self.fetch(method, entry["url"], bearer=False, **kwargs)

    def remove(self, entry):
        self.request("DELETE", "/file-access/" + entry["id"], expected=204, cookie=False)


@contextmanager
def record(client, path, purpose="download"):
    entry = client.create(path, purpose)
    try:
        yield entry
    finally:
        # Cleanup errors must remain visible, but not replace an earlier failure.
        active_error = sys.exc_info()[0] is not None
        try:
            client.remove(entry)
        except Exception:
            if not active_error:
                raise


def secure_headers(headers):
    cache = headers.get("Cache-Control", "").lower()
    require("private" in cache and "no-store" in cache, "file response may be cached")
    require(headers.get("X-Content-Type-Options") == "nosniff", "missing nosniff")
    require(headers.get("Referrer-Policy") == "no-referrer", "missing no-referrer")
    require(headers.get("Content-Encoding", "identity") == "identity", "file was compressed")
    require(headers.get("Accept-Ranges") == "bytes", "missing byte range support")


def fixtures(root, account):
    files = root / "files"
    files.mkdir(mode=0o755)
    block = hashlib.shake_256(b"strixmaid-A4-file-access-v1").digest(65536)
    payload = block * 48 + block[:137]
    paths = {"small": files / "bytes-\u9a8c\u6536.bin", "empty": files / "empty.bin",
             "sparse": files / "large-sparse.bin", "denied": files / "root-only.bin"}
    paths["small"].write_bytes(payload)
    paths["empty"].touch()
    size = 4 * 1024**3 + len(block) + 137
    with paths["sparse"].open("wb") as stream:
        stream.write(block)
        stream.seek(size - len(block))
        stream.write(block)
    for name in ("small", "empty", "sparse"):
        os.chown(paths[name], account.pw_uid, account.pw_gid)
        paths[name].chmod(0o600)
    fd = os.open(paths["denied"], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(b"root-owned acceptance fixture; must never reach the user worker\n")
    st = paths["denied"].stat()
    require(st.st_uid == 0 and stat.S_IMODE(st.st_mode) == 0o600, "invalid root-only fixture")
    sparse_stat = paths["sparse"].stat()
    require(sparse_stat.st_size == size and sparse_stat.st_blocks * 512 < 2*MIB,
            "large fixture is not sparse")
    metric("fixtures", bytes=len(payload), sha256=hashlib.sha256(payload).hexdigest(),
           sparse_bytes=size, sparse_allocated_bytes=sparse_stat.st_blocks * 512)
    return files, paths, payload, block, size


def protocol_checks(client, other, paths, payload, tail, size):
    with record(client, paths["small"]) as entry:
        client.content(entry, cookie=False, expected=401)
        client.fetch("GET", entry["url"], cookie=False, expected=401)  # Bearer alone
        body, h = client.content(entry, headers={"Accept-Encoding": "gzip, br"})
        secure_headers(h)
        require(h.get("Content-Disposition", "").startswith("attachment;"), "download not attachment")
        require("filename*=UTF-8''" in h.get("Content-Disposition", ""), "missing encoded filename")
        require(int(h.get("Content-Length", -1)) == len(payload), "wrong full length")
        require(hashlib.sha256(body).digest() == hashlib.sha256(payload).digest(), "full hash mismatch")
        _, head = client.content(entry, method="HEAD")
        require(int(head.get("Content-Length", -1)) == len(payload), "wrong HEAD length")
        etag = h.get("ETag")
        ranges = [("bytes=0-0", 0, 1), ("bytes=65531-65549", 65531, 65550),
                  ("bytes=1048571-1048611", 1048571, 1048612),
                  (f"bytes={len(payload)-37}-", len(payload)-37, len(payload)),
                  ("bytes=-113", len(payload)-113, len(payload))]
        for value, start, stop in ranges * 2:
            part, rh = client.content(entry, headers={"Range": value}, expected=206)
            require(part == payload[start:stop], "range bytes mismatch")
            require(rh.get("Content-Range") == f"bytes {start}-{stop-1}/{len(payload)}"
                    and int(rh.get("Content-Length", -1)) == stop-start, "wrong range metadata")
        for value in (f"bytes={len(payload)}-", "bytes=18446744073709551615-"):
            _, rh = client.content(entry, headers={"Range": value}, expected=416)
            require(rh.get("Content-Range") == f"bytes */{len(payload)}", "wrong 416 length")
        validators = ['"not-a-matching-version"', 'W/"weak-validator"',
                      "Thu, 01 Jan 1970 00:00:00 GMT"]
        if etag and etag.startswith("W/"):
            validators.append(etag)
        for validator in validators:
            whole, rh = client.content(entry, headers={"Range": "bytes=11-30", "If-Range": validator})
            require(whole == payload and "Content-Range" not in rh, "unverifiable If-Range not full 200")
        whole, _ = client.content(entry, headers={"Range": "bytes=0-0,4-4"})
        require(whole == payload, "multi-range must use documented full-200 fallback")
        # Multiple ordinary cookies are valid; duplicate file credentials are ambiguous.
        body, _ = client.content(entry, headers={"Cookie": "unrelated=1; " + client.cookie + "; other=2"})
        require(body == payload, "unrelated cookies broke file authentication")
        own_cookie = client.cookie
        with record(client, paths["empty"], "preview"):
            require(client.cookie == own_cookie, "same-session create did not reuse file cookie")
        with record(other, paths["small"], "preview") as other_entry:
            require(other.token != client.token and other.cookie != client.cookie,
                    "second PAM login did not produce a distinct session/credential")
            other.content(entry, expected=DENIED)
            client.content(other_entry, expected=DENIED)
            for combined in (client.cookie + "; " + other.cookie,
                             other.cookie + "; " + client.cookie,
                             client.cookie + "; " + client.cookie):
                client.content(entry, headers={"Cookie": combined}, expected=401)
            route = "/file-access/" + entry["id"]
            other.request("POST", route + "/renew", expected=DENIED)
            other.request("DELETE", route, expected=DENIED)
            client.request("GET", "/auth/session", bearer=False, expected=401)
            client.request("POST", "/file-access", {"path": str(paths["small"]), "purpose": "preview"},
                           bearer=False, expected=401)
            client.request("POST", route + "/renew", bearer=False, expected=401)
            client.request("DELETE", route, bearer=False, expected=401)
        # Control-plane renewal must be authorized by Bearer, like DELETE.
        renewed = client.request("POST", route + "/renew", cookie=False)
        require(renewed["id"] == entry["id"] and renewed["url"] == entry["url"]
                and 0 < renewed["expires_in_secs"] <= 600, "invalid renewal result")
        client.accept_cookie(client.last_headers)
        require(client.content(entry)[0] == payload, "renewed entry lost content")
    client.content(entry, expected=DENIED)
    passed("hash, repeated Range 206/416, If-Range 200, HEAD, cookies, session isolation, renew/delete")

    with record(client, paths["sparse"]) as entry:
        for value, offset, expected in [(f"bytes={size-len(tail)}-", size-len(tail), tail),
                                        ("bytes=-137", size-137, tail[-137:]),
                                        ("bytes=4294967290-4294967310", 4294967290, b"\0" * 21)]:
            body, h = client.content(entry, headers={"Range": value}, expected=206)
            require(body == expected, "sparse file large-offset bytes mismatch")
            require(h.get("Content-Range") == f"bytes {offset}-{offset+len(expected)-1}/{size}",
                    "large-offset Content-Range truncated")
        _, h = client.content(entry, method="HEAD")
        require(int(h.get("Content-Length", -1)) == size, "large HEAD length truncated")
    with record(client, paths["empty"]) as entry:
        for method in ("HEAD", "GET"):
            body, h = client.content(entry, method=method)
            require(body == b"" and h.get("Content-Length") == "0", "empty response incorrect")
        _, h = client.content(entry, headers={"Range": "bytes=0-"}, expected=416)
        require(h.get("Content-Range") == "bytes */0", "empty file range incorrect")
    with record(client, paths["denied"]) as entry:
        client.content(entry, expected=403)
        client.content(entry, method="HEAD", expected=403)
    passed("root-owned 0600 denied with 403; >4GiB sparse tail/boundary and empty GET/HEAD")


def snapshot(server_pid, files):
    # /proc children must include every thread: helpers may be spawned off-main.
    pending, seen, rows = [server_pid], set(), {}
    while pending:
        pid = pending.pop()
        if pid in seen:
            continue
        seen.add(pid)
        proc = Path(f"/proc/{pid}")
        try:
            for task in (proc / "task").iterdir():
                try:
                    pending.extend(int(p) for p in (task / "children").read_text().split())
                except FileNotFoundError:
                    pass
            status = dict(line.split(":", 1) for line in (proc / "status").read_text().splitlines())
            fds = list((proc / "fd").iterdir())
            file_fds = 0
            for fd in fds:
                try:
                    target = os.readlink(fd)
                    file_fds += target.startswith(str(files) + "/")
                except FileNotFoundError:
                    pass
            rows[pid] = {"rss_kib": int(status.get("VmRSS", "0 kB").split()[0]),
                         "anon_kib": int(status.get("RssAnon", "0 kB").split()[0]),
                         "mapped_file_kib": int(status.get("RssFile", "0 kB").split()[0]),
                         "fds": len(fds), "file_fds": file_fds,
                         "threads": int(status["Threads"])}
        except (FileNotFoundError, ProcessLookupError):
            continue
    require(server_pid in rows, "isolated server exited")
    return {key: sum(row[key] for row in rows.values())
            for key in ("rss_kib", "anon_kib", "mapped_file_kib", "fds", "file_fds", "threads")} | {"processes": len(rows)}


def memory_details(server_pid, label, cycle=0):
    """Read only /proc accounting metadata, never process memory or auth logs.

    smaps is intentionally sampled at phase boundaries, outside timed HTTP
    requests: parsing every VMA at 10 Hz would perturb the workload itself.
    Anonymous Private_Dirty means unnamed/[heap]/[stack]/[anon*] VMAs; total
    Anonymous also includes anonymous COW pages within file-backed mappings.
    These counters locate retention but cannot prove which Rust object owns it.
    """
    processes, grouped = [], {}
    descendants = lifecycle.descendants(server_pid)
    for pid in [server_pid] + sorted(descendants):
        proc = Path(f"/proc/{pid}")
        try:
            status = dict(line.split(":", 1) for line in (proc / "status").read_text().splitlines())
            name = status["Name"].strip()
            role = ("server" if pid == server_pid else "helper" if name.startswith("strixmaid-helpe")
                    else "user_worker" if name == "strixmaid" else "other")
            counts = {"rss_kib": int(status.get("VmRSS", "0").split()[0]),
                      "anon_rss_kib": int(status.get("RssAnon", "0").split()[0]),
                      "mapped_file_rss_kib": int(status.get("RssFile", "0").split()[0]),
                      "threads": int(status["Threads"]), "private_dirty_kib": 0,
                      "anon_huge_pages_kib": 0,
                      "anonymous_kib": 0, "anonymous_private_dirty_kib": 0,
                      "heap_private_dirty_kib": 0, "stack_private_dirty_kib": 0,
                      "unlabelled_private_dirty_kib": 0}
            category = None
            for line in (proc / "smaps").read_text().splitlines():
                if re.match(r"^[0-9a-f]+-[0-9a-f]+ ", line):
                    fields = line.split(None, 5)
                    path = fields[5] if len(fields) == 6 else ""
                    category = ("unlabelled" if not path else "heap" if path == "[heap]"
                                else "stack" if path.startswith("[stack")
                                else "anon" if path.startswith("[anon") else "file")
                elif line.startswith("Private_Dirty:"):
                    value = int(line.split()[1])
                    counts["private_dirty_kib"] += value
                    if category != "file":
                        counts["anonymous_private_dirty_kib"] += value
                    if category in ("unlabelled", "heap", "stack"):
                        counts[category + "_private_dirty_kib"] += value
                elif line.startswith("Anonymous:"):
                    counts["anonymous_kib"] += int(line.split()[1])
                elif line.startswith("AnonHugePages:"):
                    counts["anon_huge_pages_kib"] += int(line.split()[1])
            processes.append({"pid": pid, "role": role, **counts})
            group = grouped.setdefault(role, {"processes": 0, **{key: 0 for key in counts}})
            group["processes"] += 1
            for key, value in counts.items():
                group[key] += value
        except (FileNotFoundError, ProcessLookupError):
            continue
    require(any(row["pid"] == server_pid for row in processes), "server exited during smaps sampling")
    metric("memory_detail", phase=label, cycle=cycle, by_role=grouped, processes=processes)


class Sampler:
    def __init__(self, pid, files):
        self.pid, self.files = pid, files
        self.peak = snapshot(pid, files)
        self.samples = [self.peak]
        self.error = None
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.sample, daemon=True)

    def sample(self):
        try:
            while not self.stop.wait(0.1):
                current = snapshot(self.pid, self.files)
                self.samples.append(current)
                self.peak = {key: max(value, current[key]) for key, value in self.peak.items()}
        except Exception as error:
            self.error = type(error).__name__

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.thread.join(timeout=2)
        self.peak = {key: max(row[key] for row in self.samples) for key in self.peak}
        metric("sampled_peak", **self.peak)


def recover(pid, files, baseline, args, label):
    latest = None

    def recovered():
        nonlocal latest
        latest = snapshot(pid, files)
        return (latest["file_fds"] == 0 and latest["fds"] <= baseline["fds"] + args.fd_slack
                and latest["rss_kib"] <= baseline["rss_kib"] + args.rss_retained_mib * 1024)

    try:
        wait_for(recovered, args.recovery_timeout, label + " fd/RSS recovered")
    finally:
        metric(label, baseline=baseline, after=latest,
               fd_slack=args.fd_slack, rss_retained_mib=args.rss_retained_mib)


def control_latency(client, files, count=8):
    results = {"session_ms": [], "worker_list_ms": []}
    for _ in range(count):
        for key, route in (("session_ms", "/auth/session"),
                           ("worker_list_ms", "/files?path=" + quote(str(files), safe=""))):
            start = time.monotonic()
            client.request("GET", route, limit=MIB)
            results[key].append((time.monotonic() - start) * 1000)
    return {key: {"max": round(max(values), 2),
                  "p95": round(sorted(values)[min(len(values)-1, int(len(values)*0.95))], 2),
                  "samples": len(values)} for key, values in results.items()}


def hold(client, entry, size):
    transfer = client.open("GET", entry["url"], bearer=False, slow=True)
    try:
        require(transfer.status == 200, f"held stream expected 200, got {transfer.status}")
        require(int(transfer.headers.get("Content-Length", -1)) == size, "held stream wrong length")
        return transfer
    except BaseException:
        transfer.close()
        raise


def concurrency_checks(clients, paths, files, size, pid, args):
    entries = [client.create(paths["sparse"]) for client in clients]
    held = []
    findings, idle_samples = [], []
    try:
        # Warm every worker before measuring resource deltas, then wait for pumps.
        for client, entry in zip(clients, entries):
            client.content(entry, headers={"Range": "bytes=0-31"}, expected=206)
        wait_for(lambda: snapshot(pid, files)["file_fds"] == 0, 10, "warm-up file handles closed")
        baseline = snapshot(pid, files)
        memory_details(pid, "before_node_load")
        idle_latency = control_latency(clients[0], files)
        metric("control_idle", **idle_latency)
        with Sampler(pid, files) as sampler:
            for cycle in range(args.cancel_cycles):
                for _ in range(4):
                    held.append(hold(clients[0], entries[0], size))
                clients[0].content(entries[0], expected=409)
                # Reclaim a slot while other streams remain held, then reuse it.
                held.pop().close()

                def replacement():
                    transfer = clients[0].open("GET", entries[0]["url"], bearer=False, slow=True)
                    # The node lease may drop before the worker receives EOF;
                    # its independent permit can briefly report Unavailable.
                    if transfer.status in (409, 503):
                        transfer.close()
                        return False
                    if transfer.status != 200:
                        status = transfer.status
                        transfer.close()
                        raise RuntimeError(f"session slot replacement expected 200/409/503, got {status}")
                    held.append(transfer)
                    return True

                wait_for(replacement, args.recovery_timeout, "cancelled session stream slot reusable")
                for transfer in held:
                    transfer.close()
                held.clear()
                recover(pid, files, baseline, args, f"session_cancel_{cycle+1}")
            passed("per-session fifth stream denied (409); repeated disconnect returns slots")

            for cycle in range(1, args.node_cycles + 1):
                sample_start = len(sampler.samples)
                started = time.monotonic()
                for client, entry in zip(clients[:8], entries[:8]):
                    for _ in range(4):
                        held.append(hold(client, entry, size))
                require(time.monotonic()-started < 15, "stream setup too slow for 30s stall deadline")
                # Ninth owner has no session slots in use: isolate the node cap.
                clients[8].content(entries[8], expected=409)
                latency = control_latency(clients[0], files)
                live = snapshot(pid, files)
                sampler.samples.append(live)
                require(live["file_fds"] >= 32, "32 paused streams were not simultaneously alive")
                metric("streams_32", cycle=cycle, resources=live, control=latency,
                       elapsed_secs=round(time.monotonic()-started, 3))
                memory_details(pid, "streams_32", cycle)
                for value in latency.values():
                    require(value["max"] <= args.control_max_ms, "control API exceeded latency budget")
                held.pop().close()

                def node_replacement():
                    transfer = clients[8].open("GET", entries[8]["url"], bearer=False, slow=True)
                    if transfer.status in (409, 503):
                        transfer.close()
                        return False
                    if transfer.status != 200:
                        transfer.close()
                        raise RuntimeError(f"node slot replacement expected 200/409/503, got {transfer.status}")
                    held.append(transfer)
                    return True

                wait_for(node_replacement, args.recovery_timeout, "cancelled node stream slot reusable")
                clients[8].content(entries[8], expected=409)
                for transfer in held:
                    transfer.close()
                held.clear()
                # Observe thread-pool cooldown separately from fd cancellation.
                # Collect later rounds even if RSS fails, so a stable allocator
                # plateau is distinguishable from growing live-stream resources.
                cancelled = time.monotonic()
                wait_for(lambda: snapshot(pid, files)["file_fds"] == 0,
                         args.recovery_timeout, f"node round {cycle} file handles closed")
                checkpoints = sorted({0, min(10, args.node_cooldown),
                                      min(30, args.node_cooldown), args.node_cooldown})
                for second in checkpoints:
                    while time.monotonic() < cancelled + second:
                        time.sleep(max(0, min(0.5, cancelled + second - time.monotonic())))
                    after = snapshot(pid, files)
                    metric("node_cooldown", cycle=cycle, elapsed_secs=second, resources=after)
                    memory_details(pid, f"cooldown_{second}s", cycle)
                idle_samples.append(after)
                require(after["fds"] <= baseline["fds"] + args.fd_slack
                        and after["file_fds"] == 0, "node cancellation leaked descriptors")
                peak = max(row["rss_kib"] for row in sampler.samples[sample_start:])
                metric("node_round", cycle=cycle, peak_rss_kib=peak,
                       idle_rss_kib=after["rss_kib"], baseline_rss_kib=baseline["rss_kib"])
                warm_rss = idle_samples[0]["rss_kib"]
                if cycle == 1:
                    metric("cold_rss_retention", rss_delta_kib=warm_rss-baseline["rss_kib"],
                           peak_delta_kib=peak-baseline["rss_kib"], warm_baseline_kib=warm_rss)
                else:
                    if peak > warm_rss + args.rss_peak_mib * 1024:
                        findings.append(f"round {cycle}: peak RSS exceeds {args.rss_peak_mib} MiB above warm baseline")
                    if after["rss_kib"] > warm_rss + args.rss_retained_mib * 1024:
                        findings.append(f"round {cycle}: idle RSS exceeds {args.rss_retained_mib} MiB above warm baseline")
        require(sampler.error is None, "resource sampling failed")
        growth = idle_samples[-1]["rss_kib"] - idle_samples[0]["rss_kib"]
        max_growth = max(row["rss_kib"] for row in idle_samples) - idle_samples[0]["rss_kib"]
        metric("node_idle_growth", rounds=len(idle_samples), rss_growth_kib=growth,
               max_rss_growth_kib=max_growth)
        if max_growth > args.rss_growth_mib * 1024:
            findings.append(f"repeated idle RSS grew more than {args.rss_growth_mib} MiB")
        passed("node 33rd stream denied (409), node slot reuse, fd recovery and responsive worker control")
        for finding in findings:
            print("RESOURCE_BUDGET " + finding, flush=True)
        return findings
    finally:
        for transfer in held:
            transfer.close()
        for client, entry in zip(clients, entries):
            client.remove(entry)


def logout_check(client, survivor, paths, payload, files, size, pid, args):
    with record(survivor, paths["small"]) as survivor_entry:
        entry = client.create(paths["sparse"])
        baseline = snapshot(pid, files)
        before = set(lifecycle.descendants(pid))
        with hold(client, entry, size) as transfer:
            client.request("POST", "/auth/logout", expected=204)
            client.request("GET", "/auth/session", expected=401)
            client.content(entry, expected=401)
            # Drain queued bytes with a strict byte/time bound. A socket timeout
            # is failure, never evidence of successful cancellation.
            count, truncated = 0, False
            deadline = time.monotonic() + args.recovery_timeout
            while count <= 16*MIB and time.monotonic() < deadline:
                try:
                    chunk = transfer.response.read(65536)
                except http.client.IncompleteRead as error:
                    count += len(error.partial)
                    truncated = True
                    break
                except (ConnectionResetError, http.client.RemoteDisconnected):
                    truncated = True
                    break
                if not chunk:
                    truncated = True
                    break
                count += len(chunk)
            require(truncated and count < size, "logout did not promptly interrupt live HTTP body")
            metric("logout_cancel", drained_bytes=count, advertised_bytes=size)
        wait_for(lambda: len(before - set(lifecycle.descendants(pid))) >= 2,
                 args.recovery_timeout, "logout reaps session helper and worker")
        require(survivor.content(survivor_entry)[0] == payload, "logout damaged another session")
        recover(pid, files, baseline, args, "logout")
    passed("logout rejects old token/cookie, truncates active stream; second session survives")


def check_secrets(root, unit):
    logs = run("journalctl", "-u", unit, "--no-pager", "-o", "cat")
    require(all(secret not in logs for secret in SECRETS), "credential found in isolated service logs")
    for path in (root / "data").glob("strixmaid.db*"):
        data = path.read_bytes()
        require(all(secret.encode() not in data for secret in SECRETS),
                "plaintext credential found in database/WAL")
    passed("password, Bearer and file-cookie secrets absent from trace journal and database/WAL")


def diagnostic_hold(pid, unit, clients, seconds):
    """Keep only this test service alive for external /proc investigation.

    SIGUSR1 releases the hold and lets normal teardown continue. A finite guard
    prevents an abandoned verifier from keeping test credentials alive forever.
    No credentials or journal contents are printed as part of diagnostics.
    """
    metric("diagnostic_hold", unit=unit, server_pid=pid, verifier_pid=os.getpid(),
           max_seconds=seconds, release_signal="SIGUSR1")
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline and not DIAGNOSTIC_RELEASE.is_set():
        memory_details(pid, "diagnostic_hold")
        # Keep existing sessions alive without creating/reopening any files.
        for client in clients:
            try:
                client.request("GET", "/auth/session", expected=(200, 401))
            except (OSError, RuntimeError, http.client.HTTPException):
                pass
        DIAGNOSTIC_RELEASE.wait(min(30, max(0, deadline - time.monotonic())))
    print("DIAGNOSTIC hold released or guard elapsed; normal cleanup resumes", flush=True)


def read_password(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "r") as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode) and stat.S_IMODE(info.st_mode) == 0o600,
                "password file must be a regular mode-0600 file (no symlink)")
        require(info.st_size <= 65536, "password file is unexpectedly large")
        password = stream.read().strip()
    require(bool(password), "password file is empty")
    SECRETS.add(password)
    return password


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("user")
    parser.add_argument("password_file", type=Path)
    parser.add_argument("--server", type=Path, default=Path("/usr/local/bin/strixmaid"))
    parser.add_argument("--helper", type=Path, default=Path("/usr/local/bin/strixmaid-helper"))
    parser.add_argument("--idle-timeout", type=int, default=900)
    parser.add_argument("--elevated-timeout", type=int, default=30)
    parser.add_argument("--http-timeout", type=float, default=5)
    parser.add_argument("--recovery-timeout", type=float, default=10)
    parser.add_argument("--control-max-ms", type=float, default=1000)
    parser.add_argument("--cancel-cycles", type=int, default=3)
    parser.add_argument("--node-cycles", type=int, default=3)
    parser.add_argument("--node-cooldown", type=int, default=60,
                        help="seconds observed after each 32-stream cancellation (default: 60)")
    parser.add_argument("--fd-slack", type=int, default=8)
    parser.add_argument("--rss-peak-mib", type=int, default=128, help="peak delta budget above warm baseline")
    parser.add_argument("--rss-retained-mib", type=int, default=64, help="idle delta budget above warm baseline")
    parser.add_argument("--rss-growth-mib", type=int, default=32, help="maximum multi-round idle growth budget")
    parser.add_argument("--diagnostic-hold-secs", type=int, default=0,
                        help="keep owned service on failure for /proc diagnosis; SIGUSR1 releases early")
    args = parser.parse_args()
    require(sys.platform.startswith("linux") and os.geteuid() == 0
            and os.environ.get("STRIXMAID_TEST_VM") == "1",
            "run as root in dedicated Linux VM with STRIXMAID_TEST_VM=1")
    require(args.idle_timeout >= 60 and args.elevated_timeout >= 30, "invalid session timeouts")
    require(all(value > 0 for value in (args.http_timeout, args.recovery_timeout,
                args.control_max_ms, args.cancel_cycles, args.node_cycles, args.node_cooldown,
                args.rss_peak_mib, args.rss_retained_mib, args.rss_growth_mib))
            and args.fd_slack >= 0 and args.diagnostic_hold_secs >= 0, "invalid verification budgets")
    require(args.node_cycles >= 3, "at least three node rounds are required to assess RSS growth")
    account = lifecycle.pwd.getpwnam(args.user)
    require(account.pw_uid != 0, "use an existing disposable non-root account")
    password = read_password(args.password_file)
    for binary in (args.server, args.helper):
        require(binary.is_absolute() and binary.is_file() and os.access(binary, os.X_OK),
                "server/helper must be existing absolute executable paths")
    group = lifecycle.grp.getgrgid(account.pw_gid).gr_name
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    with tempfile.TemporaryDirectory(prefix="strixmaid-file-check-", dir="/tmp") as scratch:
        root = Path(scratch)
        # Workers need traverse permission; sensitive config/data remain private.
        root.chmod(0o711)
        (root / "data").mkdir(mode=0o700)
        files, paths, payload, tail, size = fixtures(root, account)
        config = root / "config.toml"
        config.write_text(
            f'listen = "127.0.0.1:{port}"\ndata_dir = {json.dumps(str(root / "data"))}\n'
            f'helper_path = {json.dumps(str(args.helper))}\n'
            f'[session]\nelevate_groups = [{json.dumps(group)}]\n'
            f'idle_timeout_secs = {args.idle_timeout}\n'
            f'elevated_idle_timeout_secs = {args.elevated_timeout}\n'
            f'[files]\nallowed_roots = [{json.dumps(str(files))}]\n')
        config.chmod(0o600)
        run(str(args.server), "--config", str(config), "--check-config", "serve")
        unit = root.name + ".service"
        pid, clients, held_for_diagnosis = None, [], False
        try:
            run("systemd-run", "--unit=" + unit, "--service-type=exec", "--collect",
                "--property=KillMode=control-group", "--property=TimeoutStopSec=10",
                "--property=RuntimeMaxSec=" + str(max(900, args.node_cycles * (args.node_cooldown + 30)
                                                       + args.diagnostic_hold_secs + 180)),
                "--setenv=RUST_LOG=trace",
                str(args.server), "--config", str(config), "serve")
            pid = int(run("systemctl", "show", unit, "-p", "MainPID", "--value"))
            require(pid > 1, "isolated server not started")
            client = Client(port, args.http_timeout)

            def healthy():
                try:
                    return client.request("GET", "/health")["status"] == "ok"
                except (OSError, RuntimeError, http.client.HTTPException):
                    return False

            wait_for(healthy, 30, "isolated transient server healthy")
            clients = [client] + [Client(port, args.http_timeout) for _ in range(8)]
            for item in clients:
                item.authenticate(args.user, password)
                require(item.request("GET", "/auth/session")["session_opened"], "PAM session not open")
            require(len({item.token for item in clients}) == 9, "PAM sessions not distinct")
            workers = {p: row for p, row in lifecycle.descendants(pid).items() if row[2] == "strixmaid"}
            require(len(workers) == 9, "expected nine real PAM workers")
            for worker_pid in workers:
                status = Path(f"/proc/{worker_pid}/status").read_text()
                uids = next(line.split()[1:] for line in status.splitlines() if line.startswith("Uid:"))
                require(all(int(uid) == account.pw_uid for uid in uids), "worker has incorrect real/effective/saved/fs UID")
            passed(f"nine distinct PAM sessions, real workers with UID {account.pw_uid}")
            protocol_checks(clients[0], clients[1], paths, payload, tail, size)
            resource_findings = concurrency_checks(clients, paths, files, size, pid, args)
            if resource_findings and args.diagnostic_hold_secs:
                held_for_diagnosis = True
                diagnostic_hold(pid, unit, clients, args.diagnostic_hold_secs)
            logout_check(clients[0], clients[1], paths, payload, files, size, pid, args)
            for item in clients[1:]:
                item.request("POST", "/auth/logout", expected=204)
            wait_for(lambda: not lifecycle.descendants(pid), 10, "all test helpers/workers reaped")
            check_secrets(root, unit)
            require(not resource_findings, "; ".join(resource_findings))
            passed("A4 Linux live file-access acceptance complete")
        except Exception:
            if pid and not held_for_diagnosis and args.diagnostic_hold_secs:
                diagnostic_hold(pid, unit, clients, args.diagnostic_hold_secs)
            raise
        finally:
            # Only our unique transient unit: never touch the user's main service.
            result = subprocess.run(["systemctl", "stop", unit], stdout=subprocess.PIPE,
                                    stderr=subprocess.PIPE, timeout=25, check=False)
            require(result.returncode == 0, "could not stop owned transient service")
            print("CLEANUP owned service stopped; /tmp fixtures/config/database removed on exit", flush=True)


def interrupted(_signum, _frame):
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    if hasattr(signal, "SIGUSR1"):
        signal.signal(signal.SIGUSR1, lambda *_: DIAGNOSTIC_RELEASE.set())
    try:
        main()
    except KeyboardInterrupt:
        print("FAIL interrupted; owned service cleanup attempted", file=sys.stderr)
        sys.exit(130)
    except Exception as error:
        # Never emit arbitrary server bodies, command stderr or exception reprs:
        # those could contain PAM responses, Authorization or Set-Cookie values.
        message = str(error) if isinstance(error, RuntimeError) else type(error).__name__
        for secret in sorted(SECRETS, key=len, reverse=True):
            message = message.replace(secret, "[REDACTED]")
        location = traceback.extract_tb(error.__traceback__)[-1]
        print(f"FAIL {message} ({Path(location.filename).name}:{location.lineno})", file=sys.stderr)
        sys.exit(1)
