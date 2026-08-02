#!/usr/bin/env python3
"""Minimal LSP server for proxy integration tests.

Echoes back the rootUri/rootPath it received (so the test can assert the
nvim->server rewrite) and answers definition requests with locations in its
own path space (so the test can assert the server->nvim rewrite).
"""
import json
import sys


def read_frame():
    n = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        if line.lower().startswith(b"content-length:"):
            n = int(line.split(b":", 1)[1])
    if n is None:
        return None
    return json.loads(sys.stdin.buffer.read(n))


def write(msg):
    data = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(data))
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()


root_path = ""
while True:
    msg = read_frame()
    if msg is None or msg.get("method") == "exit":
        break
    if msg.get("method") == "initialize":
        root_uri = msg["params"].get("rootUri") or ""
        root_path = root_uri[len("file://"):]
        write({
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {
                "capabilities": {},
                # "saw:" prefix keeps these echo fields out of reach of the
                # proxy's reverse rewriter, so the test observes exactly what
                # the server received.
                "sawRootUri": "saw:%s" % root_uri,
                "sawRootPath": "saw:%s" % msg["params"].get("rootPath"),
            },
        })
    elif msg.get("method") == "textDocument/definition":
        write({
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {
                "uri": "file://%s/lib/def.go" % root_path,
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1},
                },
                "detail": "%s/lib/def.go" % root_path,
            },
        })
    elif "id" in msg:
        write({"jsonrpc": "2.0", "id": msg["id"], "result": None})
