#!/usr/bin/env python3
"""A stand-in for the few calls of Cloudflare's API that `omacloud agents`
makes, for scripts/connector.sh: it writes down what it was sent.

  fake-cloudflare.py <port file> <log folder>

The token it takes is "token-one"; the account "acct"; the workers.dev name "me".
Its sign-in (/oauth2/auth, /oauth2/token) gives that token to whoever asks.
"""
import base64, hashlib, json, sys
from urllib.parse import parse_qs, urlencode, urlparse
from email.parser import BytesParser
from email.policy import default
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

log = Path(sys.argv[2])
log.mkdir(parents=True, exist_ok=True)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def answer(self, status, result=None, errors=()):
        body = json.dumps({"success": status < 300, "errors": [{"message": e} for e in errors], "result": result}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def handle_any(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        # the sign-in in a browser: leave given at once, and a token for the
        # code only to whoever holds what its challenge was made from
        url = urlparse(self.path)
        if self.command == "GET" and url.path == "/oauth2/auth":
            asked = {k: v[0] for k, v in parse_qs(url.query).items()}
            (log / "oauth-asked.json").write_text(json.dumps(asked))
            self.send_response(302)
            self.send_header("Location", asked["redirect_uri"] + "?" + urlencode({"code": "code/one+two", "state": asked["state"]}))
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.command == "POST" and url.path == "/oauth2/token":
            given = {k: v[0] for k, v in parse_qs(body.decode()).items()}
            asked = json.loads((log / "oauth-asked.json").read_text())
            proof = base64.urlsafe_b64encode(hashlib.sha256(given.get("code_verifier", "").encode()).digest()).rstrip(b"=").decode()
            ok = (given.get("code") == "code/one+two" and proof == asked["code_challenge"] and given.get("client_id") == asked["client_id"]
                  and given.get("redirect_uri") == asked["redirect_uri"] and given.get("grant_type") == "authorization_code")
            answer = json.dumps({"access_token": "token-one", "token_type": "bearer"} if ok else {"error": "invalid_grant"}).encode()
            self.send_response(200 if ok else 400)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(answer)))
            self.end_headers()
            self.wfile.write(answer)
            return
        if self.headers.get("Authorization") != "Bearer token-one":
            return self.answer(403, errors=["Authentication error"])
        if not (self.headers.get("User-Agent") or "").startswith("omacloud/"):
            return self.answer(403, errors=["error code: 1010"])
        path, method = self.path, self.command
        if method == "GET" and path == "/accounts":
            return self.answer(200, [{"id": "acct", "name": "Me"}])
        script = "/accounts/acct/workers/scripts/"
        if method == "PUT" and path.startswith(script) and "/" not in path[len(script):]:
            message = BytesParser(policy=default).parsebytes(
                b"Content-Type: " + self.headers["Content-Type"].encode() + b"\r\n\r\n" + body
            )
            parts = {}
            for part in message.iter_parts():
                name = part.get_param("name", header="content-disposition")
                parts[name] = {"type": part.get_content_type(), "size": len(part.get_payload(decode=True))}
                if name == "metadata":
                    (log / "metadata.json").write_bytes(part.get_payload(decode=True))
            (log / "upload.json").write_text(json.dumps({"script": path[len(script):], "parts": parts}))
            return self.answer(200, {"id": path[len(script):]})
        if method == "POST" and path.startswith(script) and path.endswith("/subdomain"):
            (log / "subdomain.json").write_bytes(body)
            return self.answer(200, {"enabled": True})
        if method == "GET" and path == "/accounts/acct/workers/subdomain":
            return self.answer(200, {"subdomain": "me"})
        if method == "DELETE" and path.startswith(script):
            (log / "deleted").write_text(path[len(script):])
            return self.answer(200, None)
        self.answer(404, errors=[f"no such call: {method} {path}"])

    do_GET = do_PUT = do_POST = do_DELETE = handle_any


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
Path(sys.argv[1]).write_text(str(server.server_address[1]))
server.serve_forever()
