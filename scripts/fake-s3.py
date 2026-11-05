#!/usr/bin/env python3
"""The part of S3 Omacloud uses, in memory, for scripts/s3-sync.sh.

Path-style buckets; PUT with If-None-Match, GET with Range, HEAD, DELETE and
ListObjectsV2. A key is good while it's a line of the keys file, read on each
request, so a test can retire one. A line `flaky` there answers one read in 20
with a 503, as a provider having a moment does.
Signatures aren't checked: this proves how Omacloud uses S3, not that a
provider accepts it.

    fake-s3.py <port file> <keys file> <bucket>...
"""

import re
import sys
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlsplit
from xml.sax.saxutils import escape

LOCK = threading.Lock()
BUCKETS: dict[str, dict[str, bytes]] = {}
READS = [0]
KEYS_FILE = ""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def answer(self, status, body=b"", headers=None):
        self.send_response(status)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def error_xml(self, status, code):
        self.answer(status, f"<Error><Code>{code}</Code></Error>".encode(), {"Content-Type": "application/xml"})

    def body(self):
        return self.rfile.read(int(self.headers.get("Content-Length", 0)))

    def route(self):
        """The bucket, key and query; None when refused (answered already)."""
        length_read = self.command == "PUT"
        found = re.search(r"Credential=([^/]+)/", self.headers.get("Authorization", ""))
        with open(KEYS_FILE) as f:
            good = f.read().split()
        if not found or found.group(1) not in good:
            if length_read:
                self.body()
            # as R2 answers a key that was deleted
            self.error_xml(401, "Unauthorized")
            return None
        url = urlsplit(self.path)
        if self.command in ("GET", "HEAD") and "flaky" in good:
            with LOCK:
                READS[0] += 1
                if READS[0] % 20 == 0:
                    self.error_xml(503, "SlowDown")
                    return None
        bucket, _, key = url.path.lstrip("/").partition("/")
        if bucket not in BUCKETS:
            if length_read:
                self.body()
            self.error_xml(404, "NoSuchBucket")
            return None
        return bucket, unquote(key), parse_qs(url.query, keep_blank_values=True)

    def do_PUT(self):
        route = self.route()
        if route is None:
            return
        bucket, key, _ = route
        data = self.body()
        with LOCK:
            if self.headers.get("If-None-Match") == "*" and key in BUCKETS[bucket]:
                self.error_xml(412, "PreconditionFailed")
                return
            else:
                BUCKETS[bucket][key] = data
        self.answer(200, headers={"ETag": f'"{uuid.uuid4().hex}"'})

    def do_DELETE(self):
        route = self.route()
        if route is None:
            return
        bucket, key, _ = route
        with LOCK:
            BUCKETS[bucket].pop(key, None)
        self.answer(204)

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        route = self.route()
        if route is None:
            return
        bucket, key, query = route
        if not key:
            return self.list(bucket, query)
        with LOCK:
            data = BUCKETS[bucket].get(key)
        if data is None:
            return self.error_xml(404, "NoSuchKey")
        headers = {"ETag": '"x"', "Last-Modified": "Wed, 04 Nov 2026 00:00:00 GMT", "Accept-Ranges": "bytes"}
        wanted = re.fullmatch(r"bytes=(\d+)-(\d*)", self.headers.get("Range", ""))
        if wanted and self.command == "GET":
            start = int(wanted.group(1))
            end = int(wanted.group(2)) if wanted.group(2) else len(data) - 1
            end = min(end, len(data) - 1)
            headers["Content-Range"] = f"bytes {start}-{end}/{len(data)}"
            return self.answer(206, data[start : end + 1], headers)
        return self.answer(200, data, headers)

    def list(self, bucket, query):
        one = lambda name: query.get(name, [""])[0]
        prefix, delimiter = one("prefix"), one("delimiter")
        after = one("continuation-token") or one("start-after")
        limit = int(one("max-keys") or 1000)
        with LOCK:
            names = sorted(n for n in BUCKETS[bucket] if n.startswith(prefix) and n > after)
            sizes = {n: len(BUCKETS[bucket][n]) for n in names}
        contents, prefixes, last, truncated = [], [], "", False
        for name in names:
            if len(contents) + len(prefixes) >= limit:
                truncated = True
                break
            rest = name[len(prefix) :]
            if delimiter and delimiter in rest:
                common = prefix + rest.split(delimiter)[0] + delimiter
                if common not in prefixes:
                    prefixes.append(common)
                last = name
                continue
            contents.append(
                f"<Contents><Key>{escape(name)}</Key><Size>{sizes[name]}</Size>"
                "<LastModified>2026-11-04T00:00:00.000Z</LastModified><ETag>x</ETag></Contents>"
            )
            last = name
        body = (
            "<ListBucketResult>"
            + f"<IsTruncated>{'true' if truncated else 'false'}</IsTruncated>"
            + (f"<NextContinuationToken>{escape(last)}</NextContinuationToken>" if truncated else "")
            + "".join(contents)
            + "".join(f"<CommonPrefixes><Prefix>{escape(p)}</Prefix></CommonPrefixes>" for p in prefixes)
            + "</ListBucketResult>"
        )
        self.answer(200, body.encode(), {"Content-Type": "application/xml"})


def main():
    global KEYS_FILE
    port_file, KEYS_FILE, *buckets = sys.argv[1:]
    for bucket in buckets:
        BUCKETS[bucket] = {}
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    with open(port_file, "w") as f:
        f.write(str(server.server_address[1]))
    server.serve_forever()


if __name__ == "__main__":
    main()
