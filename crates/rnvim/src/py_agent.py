#!/usr/bin/env python3
"""rnvim remote agent — portable fallback.

Pushed to remotes whose platform differs from the local machine (so the local
rnvim binary itself cannot run there). Speaks the same JSON-lines protocol as
the Rust agent. Python 3.6+, stdlib only.
"""
import base64
import json
import os
import sys

PROTO = 1


def expand(p):
    p = p or "~"
    p = os.path.expanduser(p)
    if not os.path.isabs(p):
        p = os.path.join(os.path.expanduser("~"), p)
    return os.path.normpath(p)


def kind_of(p):
    if os.path.isdir(p):
        return "dir"
    if os.path.exists(p):
        return "file"
    return "missing"


def h_hello(params):
    if params.get("proto") != PROTO:
        raise ValueError(
            "protocol mismatch: client %s vs agent %s" % (params.get("proto"), PROTO)
        )
    u = os.uname()
    return {
        "agent_version": "py",
        "proto": PROTO,
        "os": u.sysname.lower(),
        "arch": u.machine,
        "home": os.path.expanduser("~"),
    }


def h_resolve(params):
    p = expand(params.get("path", "~"))
    return {"abs": p, "kind": kind_of(p)}


def h_stat(params):
    p = expand(params["path"])
    k = kind_of(p)
    size = os.path.getsize(p) if k == "file" else 0
    return {"kind": k, "size": size}


def h_read(params):
    p = expand(params["path"])
    with open(p, "rb") as f:
        data = f.read()
    return {"content_b64": base64.b64encode(data).decode("ascii"), "size": len(data)}


def h_write(params):
    p = expand(params["path"])
    data = base64.b64decode(params["content_b64"])
    d = os.path.dirname(p)
    if d:
        os.makedirs(d, exist_ok=True)
    with open(p, "wb") as f:
        f.write(data)
    return {"bytes": len(data)}


def h_list(params):
    p = expand(params["path"])
    entries = []
    for name in os.listdir(p):
        kind = "dir" if os.path.isdir(os.path.join(p, name)) else "file"
        entries.append({"name": name, "kind": kind})
    entries.sort(key=lambda e: (e["kind"] != "dir", e["name"]))
    return {"entries": entries}


HANDLERS = {
    "hello": h_hello,
    "fs.resolve": h_resolve,
    "fs.stat": h_stat,
    "fs.read": h_read,
    "fs.write": h_write,
    "fs.list": h_list,
}


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        rid = 0
        try:
            req = json.loads(line)
            rid = req.get("id", 0)
            fn = HANDLERS.get(req.get("method"))
            if fn is None:
                resp = {
                    "id": rid,
                    "error": {
                        "code": -32601,
                        "message": "unknown method: %s" % req.get("method"),
                    },
                }
            else:
                resp = {"id": rid, "result": fn(req.get("params") or {})}
        except Exception as e:  # noqa: BLE001 — every failure must become a response
            resp = {"id": rid, "error": {"code": 1, "message": str(e)}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
