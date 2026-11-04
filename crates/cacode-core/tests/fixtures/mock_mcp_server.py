#!/usr/bin/env python3
"""Tiny stdio MCP server used by CACode tests (newline-delimited JSON-RPC)."""
import json, sys

TOOLS = [
    {"name": "add", "description": "Add two integers", "inputSchema": {"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}}, "required": ["a", "b"]}, "annotations": {"readOnlyHint": True}},
    {"name": "write_note", "description": "Pretend to write a note", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "boom", "description": "Always fails", "inputSchema": {"type": "object"}},
]

def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    mid, method, p = msg.get("id"), msg.get("method"), msg.get("params") or {}
    if mid is None:
        continue  # notification
    if method == "initialize":
        # exercise server→client request handling
        send({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "mock", "version": "1"}}})
    elif method == "tools/list":
        if p.get("cursor") is None:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS[:2], "nextCursor": "page2"}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS[2:]}})
    elif method == "tools/call":
        name, a = p.get("name"), p.get("arguments") or {}
        if name == "add":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": str(a["a"] + a["b"])}]}})
        elif name == "write_note":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "noted: " + a.get("text", "")}]}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"isError": True, "content": [{"type": "text", "text": "kaboom"}]}})
    else:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "nope"}})
