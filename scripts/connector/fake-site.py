#!/usr/bin/env python3
"""A stand-in for what omacloud.computer does for accounts (server/src/paid.ts)
and for where Cloudflare's sign-in returns (/cloudflare/callback), for
scripts/connector.sh: it writes down what it was sent.

  fake-site.py <port file> <log folder>

A subscription counts as paid for while the file `paid` is in the log folder.
"""
import json, re, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

log = Path(sys.argv[2])
log.mkdir(parents=True, exist_ok=True)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def answer(self, status, body):
        body = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def handle_any(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        # where Cloudflare's sign-in returns: on to the waiting command's port
        if self.command == "GET" and self.path.startswith("/cloudflare/callback?"):
            query = self.path.split("?", 1)[1]
            port = re.search(r"(?:^|&)state=(\d+)\.", query)[1]
            self.send_response(302)
            self.send_header("Location", f"http://127.0.0.1:{port}/callback?{query}")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if not re.fullmatch(r"Bearer [0-9a-f]{64}", self.headers.get("Authorization") or ""):
            return self.answer(401, {"error": {"code": "no_key", "message": "this needs the account's key"}})
        path, method = self.path, self.command
        paid = (log / "paid").exists()
        hosted = sorted(p.stem[len("hosted-"):] for p in log.glob("hosted-*.json"))
        if method == "GET" and path == "/api/subscription":
            return self.answer(200, {
                "active": paid,
                "status": "active" if paid else "none",
                "until": 0,
                "connectors": [{"name": n, "url": f"https://oc-test-{n}.omacloud.workers.dev"} for n in hosted],
                "passkeys": 0,
            })
        if method == "POST" and path == "/api/portal":
            return self.answer(200, {"url": "https://billing.stripe.com/p/test"})
        if method == "DELETE" and path == "/api/passkeys":
            (log / "passkeys-forgotten").write_text("yes")
            return self.answer(200, {"removed": True})
        connector = re.fullmatch(r"/api/connectors/([a-z0-9-]+)", path)
        if connector and method == "DELETE":
            (log / f"hosted-{connector[1]}.json").unlink(missing_ok=True)
            (log / "removed").write_text(connector[1])
            return self.answer(200, {"removed": connector[1]})
        if connector and method == "PUT":
            if not paid:
                return self.answer(402, {"error": {"code": "not_subscribed", "message": "this needs a subscription"}})
            (log / f"hosted-{connector[1]}.json").write_bytes(body)
            return self.answer(200, {"url": f"https://oc-test-{connector[1]}.omacloud.workers.dev"})
        self.answer(404, {"error": {"code": "not_found", "message": "nothing is here"}})

    do_GET = do_PUT = do_POST = do_DELETE = handle_any


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
Path(sys.argv[1]).write_text(str(server.server_address[1]))
server.serve_forever()
