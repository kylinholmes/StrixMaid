#!/usr/bin/env python3
"""Keep a real services.changed subscriber while running the wedge checks.

Run inside the dedicated VM, after starting strixmaid and provisioning a test
account. Password comes from a mode-0600 file; neither it nor the token is logged.
Example: python3 scripts/verify/vm/dbus-subscriber.py USER PASSWORD_FILE
Requires python3-websockets, password PAM, and passwordless sudo in the test VM.
"""
import asyncio
import json
import os
from pathlib import Path
import sys
import time
import urllib.request

from websockets.asyncio.client import connect

BASE = "http://127.0.0.1:9700"
ROOT = Path(__file__).resolve().parents[1]


def login(user, password):
    def post(endpoint, data):
        req = urllib.request.Request(
            BASE + "/api/v1/auth/" + endpoint,
            data=json.dumps(data).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=20) as response:
            return json.load(response)

    response = post("start", {"username": user})
    for _ in range(5):
        response = post("respond", {
            "session": response["session"],
            "responses": [
                {"id": p["id"], "value": password if p["style"] == "prompt" else ""}
                for p in response.get("prompts", [])
            ],
        })
        if response.get("status") == "complete":
            return response["token"]
        if response.get("status") != "more":
            raise RuntimeError("PAM test login failed")
    raise RuntimeError("PAM conversation exceeded five rounds")


async def command(*args, timeout=180):
    proc = await asyncio.create_subprocess_exec(*map(str, args))
    try:
        rc = await asyncio.wait_for(proc.wait(), timeout)
    except BaseException:
        proc.kill()
        await proc.wait()
        raise
    if rc:
        raise RuntimeError(f"command failed ({rc}): {args[0:3]}")


async def subscribe(token, verify):
    async with connect("ws://127.0.0.1:9700/ws", subprotocols=["bearer", token]) as ws:
        await ws.send(json.dumps({"v": 1, "t": "sub", "ch": "services.changed", "id": 1, "d": {}}))
        states = {}
        batches = 0

        async def receive():
            nonlocal batches
            async for raw in ws:
                msg = json.loads(raw)
                if msg["t"] == "err":
                    raise RuntimeError(f"subscription failed: {msg['d']}")
                if msg["t"] == "data" and msg.get("ch") == "services.changed":
                    batches += 1
                    for unit in msg["d"]:
                        states.setdefault(unit["name"], set()).add(unit["active_state"])

        receiver = asyncio.create_task(receive())
        try:
            await asyncio.sleep(1)
            await verify(states)
            if receiver.done():
                await receiver
                raise RuntimeError("subscriber closed unexpectedly")
            print(f"services.changed: {batches} batches, {len(states)} unique units", flush=True)
        finally:
            receiver.cancel()
            await asyncio.gather(receiver, return_exceptions=True)


async def wait_seen(states, names, state):
    deadline = time.monotonic() + 12
    while not all(state in states.get(n, ()) for n in names):
        if time.monotonic() > deadline:
            missing = [n for n in names if state not in states.get(n, ())]
            raise RuntimeError(f"missing {state} events: {missing[:8]} ({len(missing)} total)")
        await asyncio.sleep(0.1)


async def main():
    user, password_file = sys.argv[1:]
    password_path = Path(password_file)
    if password_path.stat().st_mode & 0o077:
        raise RuntimeError("password file must be private (0600)")
    token = await asyncio.to_thread(login, user, password_path.read_text().strip())
    rounds = int(os.environ.get("ROUNDS", "20"))

    async def stress(states):
        # Installed by the documented VM setup: a unique, harmless runtime template.
        names = [f"strixmaid-dbus-probe@{i}.service" for i in range(96)]
        for iteration in range(3):
            states.clear()
            await command("sudo", "systemctl", "start", *names)
            await wait_seen(states, names, "active")
            await command("sudo", "systemctl", "stop", *names)
            await wait_seen(states, names, "inactive")
            print(f"burst {iteration + 1}: 96 active + 96 inactive events received", flush=True)
        await command("sudo", "env", f"ROUNDS={rounds}", "bash", ROOT / "dbus-wedge-stress.sh", timeout=600)
        await command("sudo", "env", "MONITOR_SECS=60", "bash", ROOT / "dbus-wedge-check.sh", timeout=120)

    await subscribe(token, stress)
    print("subscriber disconnected; waiting 35s for idle retirement", flush=True)
    await asyncio.sleep(35)

    async def resumed(states):
        name = "strixmaid-dbus-probe@resume.service"
        await command("sudo", "systemctl", "start", name)
        await wait_seen(states, [name], "active")
        await command("sudo", "systemctl", "stop", name)
        await wait_seen(states, [name], "inactive")
        print("resubscribe after retirement: active + inactive events received", flush=True)

    await subscribe(token, resumed)


if __name__ == "__main__":
    asyncio.run(main())
