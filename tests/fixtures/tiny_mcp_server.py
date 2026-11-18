#!/usr/bin/env python3
# Minimal stdio MCP server: handshake, paged tools/list, tools/call.
import json, sys, time
import os
# `--silent` accepts the handshake and then never answers, so the client-side
# timeout is exercised for real. `--name` sets the advertised server name.
SILENT = "--silent" in sys.argv
SLOW_CALL = "--slow-call" in sys.argv
NAME = "tiny"
for i, a in enumerate(sys.argv):
    if a == "--name" and i + 1 < len(sys.argv): NAME = sys.argv[i+1]

def log(m):
    if os.environ.get("MCP_TEST_LOG"):
        open(os.environ["MCP_TEST_LOG"], "a", buffering=1).write(m + "\n")

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n"); sys.stdout.flush()
    log("SENT " + json.dumps(obj)[:200])

for line in sys.stdin:
    line = line.strip()
    if not line: continue
    log("RECV " + line[:200])
    if SILENT: continue
    try: msg = json.loads(line)
    except Exception: continue
    method = msg.get("method"); mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2024-11-05",
            "capabilities":{"tools":{}},
            "serverInfo":{"name":NAME,"version":"0"}}})
        # emit a banner + a notification + a server->client request, all of
        # which a correct client must tolerate.
        send({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"hello"}})
        send({"jsonrpc":"2.0","id":999,"method":"sampling/createMessage","params":{}})
    elif method == "tools/list":
        if msg.get("params",{}).get("cursor") == "p2":
            send({"jsonrpc":"2.0","id":mid,"result":{"tools":[
                {"name":"echo/two","description":"second page","inputSchema":{"type":"object","properties":{"x":{"type":"string"}}}}]}})
        else:
            send({"jsonrpc":"2.0","id":mid,"result":{"tools":[
                {"name":"echo","description":"echo back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
                {"name":"boom","description":"always fails","inputSchema":{"type":"object"}}],
                "nextCursor":"p2"}})
    elif method == "tools/call":
        name = msg.get("params",{}).get("name")
        if SLOW_CALL and name != "boom":
            time.sleep(30)
        if name == "boom":
            send({"jsonrpc":"2.0","id":mid,"error":{"code":-32000,"message":"tool exploded"}})
        else:
            args = msg.get("params",{}).get("arguments",{})
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[
                {"type":"text","text":"ECHO:"+str(args.get("text"))},
                {"type":"text","text":"second block"}]}})
    elif mid is not None and method is None:
        log("CLIENT REPLY " + json.dumps(msg)[:200])
