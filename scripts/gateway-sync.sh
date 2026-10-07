#!/usr/bin/env bash
# Two computers sync through the Omacloud storage Worker (server/, run
# locally by wrangler) with SeaweedFS as the bucket behind; another
# account's key is shown it can't reach the first account's folder; then
# two computers sign in (with a stand-in for Google) and sync, and the
# waitlist takes a sign-in and a request from the website.
#
#   scripts/gateway-sync.sh        (needs docker, node and python3)
set -euo pipefail

cargo build -q -p omacloud
(cd server && npm install --silent)
work=$(mktemp -d)
weed=omacloud-gateway-test-$$
cleanup() {
  for f in "$work"/*.toml; do
    secret-tool clear service omacloud config "$f" 2>/dev/null || true
  done
  [ -n "${google:-}" ] && kill "$google" 2>/dev/null || true
  # the dev server and everything it started
  [ -n "${server:-}" ] && kill -- -"$server" 2>/dev/null || true
  docker rm -f "$weed" >/dev/null 2>&1 || true
  rm -rf "${work:?}"
}
trap cleanup EXIT

port_of() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
wport=$(port_of)
gport=$(port_of)
googleport=$(port_of)
upsecret=$(head -c 16 /dev/urandom | xxd -p)
cat >"$work/s3.json" <<EOF
{"identities": [{"name": "upstream", "credentials": [{"accessKey": "upstream", "secretKey": "$upsecret"}],
  "actions": ["Admin", "Read", "Write", "List", "Tagging"]}]}
EOF
docker run -d --name "$weed" -p "127.0.0.1:$wport:8333" -v "$work/s3.json:/etc/s3.json:ro" \
  chrislusf/seaweedfs server -s3 -s3.config=/etc/s3.json -dir=/data >/dev/null

# s3 METHOD PATH [QUERY]: a SigV4-signed request; prints the status, then
# the body. S3_HOST, S3_KEY, S3_SECRET, S3_REGION say where and as whom.
cat >"$work/s3.py" <<'EOF'
import os, sys, hashlib, hmac, datetime, urllib.request, urllib.error
method, path = sys.argv[1], sys.argv[2]
query = sys.argv[3] if len(sys.argv) > 3 else ""
host, key, secret, region = (os.environ[v] for v in ("S3_HOST", "S3_KEY", "S3_SECRET", "S3_REGION"))
t = datetime.datetime.now(datetime.UTC)
stamp, day = t.strftime("%Y%m%dT%H%M%SZ"), t.strftime("%Y%m%d")
h = "UNSIGNED-PAYLOAD"
canon = f"{method}\n{path}\n{query}\nhost:{host}\nx-amz-content-sha256:{h}\nx-amz-date:{stamp}\n\nhost;x-amz-content-sha256;x-amz-date\n{h}"
scope = f"{day}/{region}/s3/aws4_request"
sts = f"AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{hashlib.sha256(canon.encode()).hexdigest()}"
k = ("AWS4" + secret).encode()
for p in scope.split("/"):
    k = hmac.new(k, p.encode(), hashlib.sha256).digest()
sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
url = f"http://{host}{path}" + (f"?{query}" if query else "")
data = b"x" if method == "PUT" and path.count("/") > 1 else None
req = urllib.request.Request(url, method=method, data=data, headers={
    "x-amz-date": stamp, "x-amz-content-sha256": h,
    "authorization": f"AWS4-HMAC-SHA256 Credential={key}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={sig}"})
try:
    r = urllib.request.urlopen(req)
    status, body = r.status, r.read().decode()
except urllib.error.HTTPError as e:
    status, body = e.code, e.read().decode()
print(status)
print(body)
EOF
upstream() { S3_HOST=127.0.0.1:$wport S3_KEY=upstream S3_SECRET=$upsecret S3_REGION=us-east-1 python3 "$work/s3.py" "$@"; }
for _ in $(seq 100); do
  [ "$(upstream PUT /everyone 2>/dev/null | head -1)" = 200 ] && break
  sleep 0.3
done

master=$(head -c 32 /dev/urandom | xxd -p -c 64)
# throwaway secrets for this run only
cat >"$work/dev.vars" <<EOF
UPSTREAM_SECRET=$upsecret
GOOGLE_SECRET=google-test-secret
MASTER_KEY=$master
TURNSTILE_SECRET=turnstile-test-secret
EOF
# the Worker as deployed (server/src, server/migrations), configured for
# this run: local addresses, test values, no domains (the dev server would
# rewrite every request's Host to theirs, and Host tells sign-in from
# storage)
cat >"$work/wrangler.jsonc" <<EOF
{
  "name": "omacloud-test",
  "main": "$PWD/server/src/index.ts",
  "compatibility_date": "2026-10-05",
  "d1_databases": [{ "binding": "DB", "database_name": "omacloud", "database_id": "local",
                     "migrations_dir": "$PWD/server/migrations" }],
  "triggers": { "crons": ["*/15 * * * *"] },
  "send_email": [{ "name": "EMAIL" }],
  "ratelimits": [
    { "name": "SIGNIN_LIMIT", "namespace_id": "2101", "simple": { "limit": 20, "period": 60 } },
    { "name": "CREDENTIALS_LIMIT", "namespace_id": "2102", "simple": { "limit": 10, "period": 60 } },
    { "name": "WAITLIST_LIMIT", "namespace_id": "2103", "simple": { "limit": 5, "period": 60 } }
  ],
  "vars": {
    "PUBLIC_URL": "http://localhost:$gport", "STORAGE_HOST": "127.0.0.1:$gport", "REGION": "omacloud",
    "SIGNUPS": "invite", "DEFAULT_QUOTA": "200000000000",
    "UPSTREAM_ENDPOINT": "http://127.0.0.1:$wport", "UPSTREAM_REGION": "us-east-1",
    "UPSTREAM_BUCKET": "everyone", "UPSTREAM_KEY_ID": "upstream",
    "GOOGLE_CLIENT_ID": "test-client", "GOOGLE_AUTH_URL": "http://127.0.0.1:$googleport/auth",
    "GOOGLE_TOKEN_URL": "http://127.0.0.1:$googleport/token", "ADMIN_EMAIL": "admin@example.com",
    "TURNSTILE_SITE_KEY": "test-site", "TURNSTILE_VERIFY_URL": "http://127.0.0.1:$googleport/siteverify"
  }
}
EOF
wrangler() { (cd server && npx wrangler "$@" -c "$work/wrangler.jsonc"); }
d1() { wrangler d1 execute omacloud --local --persist-to "$work/state" --command "$1" >/dev/null; }
d1get() {
  wrangler d1 execute omacloud --local --persist-to "$work/state" --json --command "$1" |
    python3 -c 'import json,sys; r=json.load(sys.stdin)[0]["results"][0]; print(next(iter(r.values())))'
}
wrangler d1 migrations apply omacloud --local --persist-to "$work/state" >/dev/null 2>&1
set -m # its own process group, so cleanup stops all of it
(cd server && exec npx wrangler dev -c "$work/wrangler.jsonc" --local --ip 127.0.0.1 --port "$gport" \
  --persist-to "$work/state" --env-file "$work/dev.vars" --test-scheduled) >"$work/server.log" 2>&1 &
server=$!
set +m
for _ in $(seq 100); do curl -sf "http://localhost:$gport/health" >/dev/null && break; sleep 0.3; done
curl -sf "http://localhost:$gport/health" >/dev/null || { cat "$work/server.log"; exit 1; }

# two accounts with a key each, as a sign-in makes them
account() { # id key
  d1 "INSERT INTO accounts (id, google_sub, email, created, quota) VALUES ('$1', '$1', '$1@example.com', 0, 1000000000);
    INSERT INTO keys (id, account, created) VALUES ('$2', '$1', 0);"
}
secret() {
  python3 -c 'import hmac,hashlib,sys; print(hmac.new(bytes.fromhex(sys.argv[1]), b"omacloud s3 secret\0"+sys.argv[2].encode(), hashlib.sha256).hexdigest())' "$master" "$1"
}
a=u00000000000000aa
b=u00000000000000bb
account $a OCAAAAAAAAAAAAAAAAAA
account $b OCBBBBBBBBBBBBBBBBBB

S3_ENDPOINT="http://127.0.0.1:$gport" S3_BUCKET=$a S3_REGION=omacloud \
  S3_KEY=OCAAAAAAAAAAAAAAAAAA S3_SECRET=$(secret OCAAAAAAAAAAAAAAAAAA) \
  scripts/s3-sync.sh || { echo "--- server log"; tail -30 "$work/server.log"; exit 1; }

# everything a wrote is in its folder of the bucket behind
listing=$(upstream GET /everyone list-type=2)
keys=$(grep -o '<Key>[^<]*</Key>' <<<"$listing" || true)
[ -n "$keys" ] || { echo "nothing in the bucket behind"; exit 1; }
if grep -v "<Key>$a/" <<<"$keys"; then echo "written outside a's folder"; exit 1; fi

as_b() { S3_HOST=127.0.0.1:$gport S3_KEY=OCBBBBBBBBBBBBBBBBBB S3_SECRET=$(secret OCBBBBBBBBBBBBBBBBBB) S3_REGION=omacloud python3 "$work/s3.py" "$@"; }
expect() { # status method path [query]
  local want=$1; shift
  out=$(as_b "$@")
  [ "$(head -1 <<<"$out")" = "$want" ] || { echo "$* gave $out, not $want"; exit 1; }
}
expect 200 PUT /$b/hello
expect 200 GET /$b list-type=2
grep -q '<Key>hello</Key>' <<<"$out" || { echo "b's listing: $out"; exit 1; }
if grep -q "<Key>$b/" <<<"$out"; then echo "b's listing shows its folder"; exit 1; fi
expect 403 GET /$a list-type=2
expect 403 GET /$a/config
# paths out of the folder: refused, by the signature (a URL resolves the
# dots before the gateway sees them) or by the name check
refused() {
  out=$(as_b "$@")
  case $(head -1 <<<"$out") in 400|403) ;; *) echo "$* gave $out"; exit 1 ;; esac
}
refused GET /$b/../$a/config
refused GET /$b/%2E%2E/$a/config
expect 501 PUT /$b/hello acl=
expect 501 GET /$b
expect 200 GET /$b "list-type=2&prefix=..%2F$a%2F"
if grep -q '<Key>' <<<"$out"; then echo "listed outside b's folder: $out"; exit 1; fi
d1 "UPDATE accounts SET quota = 1 WHERE id = '$b'"
expect 403 PUT /$b/more
grep -q QuotaExceeded <<<"$out" || { echo "quota: $out"; exit 1; }
echo "gateway ok"

# Google, standing in: whoever $work/who names signs in; and Turnstile,
# which takes a person to be whoever says "human"
cat >"$work/google.py" <<'EOF'
import sys, json, base64, urllib.parse, http.server
who_file = sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
        who = open(who_file).read().strip()
        to = q["redirect_uri"][0] + "?" + urllib.parse.urlencode({"code": who, "state": q["state"][0]})
        self.send_response(302); self.send_header("location", to); self.end_headers()
    def do_POST(self):
        raw = self.rfile.read(int(self.headers["content-length"])).decode()
        if self.path == "/siteverify":
            ask = json.loads(raw)
            assert ask["secret"] == "turnstile-test-secret", ask
            ok = ask["response"] == "human"
            body = json.dumps({"success": ok, "hostname": "localhost"} if ok else {"success": False}).encode()
            self.send_response(200); self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
            return
        form = urllib.parse.parse_qs(raw)
        assert form["client_secret"] == ["google-test-secret"], form
        sub = form["code"][0]
        claims = {"iss": "https://accounts.google.com", "aud": "test-client", "sub": sub,
                  "email": sub + "@example.com", "email_verified": True}
        part = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
        body = json.dumps({"id_token": "e30." + part + ".sig"}).encode()
        self.send_response(200); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
EOF
python3 "$work/google.py" "$googleport" "$work/who" &
google=$!
for _ in $(seq 50); do
  python3 -c 'import socket,sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])))' "$googleport" \
    2>/dev/null && break
  sleep 0.1
done
# the browser, standing in: follows the redirects back to the computer
mkdir -p "$work/bin"
printf '#!/bin/sh\ncurl -sfL "$1" >/dev/null &\n' >"$work/bin/xdg-open"
chmod +x "$work/bin/xdg-open"
# keys stay in the config files, so the test can read them
oc() { local dev=$1; shift; PATH="$work/bin:$PATH" OMACLOUD_SERVICE_URL="http://localhost:$gport" \
  OMACLOUD_KEYRING=0 target/debug/omacloud --config "$work/$dev.toml" "$@"; }

d1 "INSERT INTO invites (email, created) VALUES ('alice@example.com', 0)"
echo alice >"$work/who"
oc ha init --hosted --folder "$work/HA" --device ha >"$work/ha.out" 2>&1 \
  || { cat "$work/ha.out"; tail -20 "$work/server.log"; exit 1; }
grep -q "signed in as alice@example.com" "$work/ha.out" || { cat "$work/ha.out"; exit 1; }
echo "from a signed-in computer" >"$work/HA/note.txt"
oc ha sync >/dev/null 2>&1
# alice's second computer signs in too, and the first approves it
oc hb init --hosted --folder "$work/HB" --device hb >"$work/hb.out" 2>&1 \
  || { cat "$work/hb.out"; exit 1; }
fp=$(oc hb device-key 2>/dev/null | sed -n 's/^fingerprint //p')
oc ha devices approve "$fp" >/dev/null 2>&1
oc hb sync >/dev/null 2>&1
grep -q "from a signed-in computer" "$work/HB/note.txt" || { echo "hb didn't get the file"; exit 1; }
# hb's daemon sits idle on the change marker, and still sees ha's change
oc hb watch --poll 2 >/dev/null 2>&1 &
watcher=$!
sleep 5
echo "while hb idles" >"$work/HA/later.txt"
oc ha sync >/dev/null 2>&1
for _ in $(seq 30); do [ -e "$work/HB/later.txt" ] && break; sleep 1; done
kill "$watcher"
[ -e "$work/HB/later.txt" ] || { echo "idle hb missed ha's change"; exit 1; }
# a lost computer: ha removes hb and changes the key; once every computer
# left is on the new key, the old keys are retired, and hb's own key no
# longer reaches the storage
read -r hbucket hbid hbsecret < <(python3 -c 'import tomllib,sys; c=tomllib.load(open(sys.argv[1],"rb"))["coordination"]; print(c["bucket"], c["access_key_id"], c["secret_access_key"])' "$work/hb.toml")
as_hb() { S3_HOST=127.0.0.1:$gport S3_KEY=$hbid S3_SECRET=$hbsecret S3_REGION=omacloud python3 "$work/s3.py" "$@"; }
[ "$(as_hb GET "/$hbucket" list-type=2 | head -1)" = 200 ] || { echo "hb's key should work before"; exit 1; }
oc ha devices revoke "$fp" >/dev/null 2>&1
oc ha bucket set-key >"$work/setkey.out" 2>&1 || { cat "$work/setkey.out"; exit 1; }
oc ha bucket status >"$work/bstatus.out" 2>&1 || { cat "$work/bstatus.out"; exit 1; }
grep -q "old keys are retired" "$work/bstatus.out" || { cat "$work/bstatus.out"; exit 1; }
out=$(as_hb GET "/$hbucket" list-type=2)
[ "$(head -1 <<<"$out")" = 403 ] || { echo "hb's key still works after retiring: $out"; exit 1; }
echo "after the key change" >"$work/HA/after.txt"
oc ha sync >/dev/null 2>&1 || { echo "ha can't sync on the new key"; exit 1; }
# a computer whose key was retired under it signs in again and carries on
oc ha bucket set-key --here-only >"$work/hereonly.out" 2>&1 || { cat "$work/hereonly.out"; exit 1; }
echo "after signing in again" >"$work/HA/again.txt"
oc ha sync >/dev/null 2>&1 || { echo "ha can't sync after signing in again"; exit 1; }
echo "key change ok"
# carol isn't invited
echo carol >"$work/who"
if oc hc init --hosted --folder "$work/HC" --device hc >"$work/hc.out" 2>&1; then
  echo "carol got in"; exit 1
fi
grep -q "on the waitlist" "$work/hc.out" || { cat "$work/hc.out"; exit 1; }
[ "$(d1get "SELECT count(*) FROM waitlist WHERE email = 'carol@example.com' AND confirmed IS NOT NULL")" = 1 ] \
  || { echo "carol isn't on the waitlist"; exit 1; }
echo "signed-in sync ok"
# a trusted contact: alice makes a card, and a computer with nothing else
# gets back in with it and her sign-in
echo alice >"$work/who"
code=$(sed -n 's/^recovery code: //p' "$work/ha.out")
card=$(OMACLOUD_RECOVERY_CODE=$code oc ha recovery contact 2>"$work/contact.err") \
  || { cat "$work/contact.err"; exit 1; }
recover() { # device card
  OMACLOUD_CONTACT_CARD=$2 oc "$1" init --hosted --contact-card --folder "$work/${1^^}" --device "$1" \
    >"$work/$1.out" 2>&1
}
recover hd "$card" || { cat "$work/hd.out"; exit 1; }
grep -q "recovery code: $code" "$work/hd.out" || { cat "$work/hd.out"; exit 1; }
oc hd sync >"$work/hdsync.out" 2>&1 || { cat "$work/hdsync.out"; exit 1; }
grep -q "from a signed-in computer" "$work/HD/note.txt" || { echo "hd didn't get the file"; exit 1; }
# a mistyped card is caught before signing in
typo=z${card:1}
[ "$typo" = "$card" ] && typo=y${card:1}
if recover hx "$typo"; then echo "a mistyped card worked"; exit 1; fi
grep -q "mistyped" "$work/hx.out" || { cat "$work/hx.out"; exit 1; }
# a new card replaces the last
OMACLOUD_RECOVERY_CODE=$code oc ha recovery contact >/dev/null 2>"$work/contact.err" \
  || { cat "$work/contact.err"; exit 1; }
if recover he "$card"; then echo "a replaced card worked"; exit 1; fi
grep -q "earlier card" "$work/he.out" || { cat "$work/he.out"; exit 1; }
# another Google account can't change alice's contact
d1 "INSERT INTO invites (email, created) VALUES ('bob@example.com', 0)"
echo bob >"$work/who"
if oc ha recovery contact --remove >"$work/bob.out" 2>&1; then echo "bob removed alice's contact"; exit 1; fi
grep -q "isn't the Google account" "$work/bob.out" || { cat "$work/bob.out"; exit 1; }
# removed: no card opens the account
echo alice >"$work/who"
oc ha recovery contact --remove >/dev/null 2>"$work/contact.err" \
  || { cat "$work/contact.err"; exit 1; }
if recover hf "$card"; then echo "a card worked with no contact"; exit 1; fi
grep -q "no trusted contact" "$work/hf.out" || { cat "$work/hf.out"; exit 1; }
echo "trusted contact ok"
# the waitlist form: Turnstile, then a link to confirm, emailed (the local
# email binding writes what it sends to files, named in the log)
ask() { # email turnstile: prints the status
  curl -s -o "$work/ask.html" -w '%{http_code}' "http://localhost:$gport/waitlist" \
    --data-urlencode "email=$1" --data-urlencode "cf-turnstile-response=$2"
}
curl -sf "http://localhost:$gport/" | grep -q 'data-sitekey="test-site"' || { echo "no form on the home page"; exit 1; }
[ "$(ask dave@example.com robot)" = 403 ] || { echo "a robot asked"; cat "$work/ask.html"; exit 1; }
[ "$(ask Dave@Example.com human)" = 200 ] && grep -q "Check your inbox" "$work/ask.html" \
  || { echo "dave couldn't ask"; cat "$work/ask.html"; exit 1; }
sent_to() { # the files the local email binding wrote for mail to $1, newest last
  grep -A6 "send_email binding called" "$work/server.log" | grep -A5 "To: .*$1" |
    sed -n 's/.*Text: \(\S*\.txt\).*/\1/p'
}
for _ in $(seq 20); do [ -n "$(sent_to dave@example.com)" ] && break; sleep 0.3; done
link=$(grep -ho 'http://localhost:[0-9]*/waitlist/confirm?token=[0-9a-f]*' $(sent_to dave@example.com) | tail -1)
[ -n "$link" ] || { echo "no confirmation email"; tail -30 "$work/server.log"; exit 1; }
[ "$(d1get "SELECT count(*) FROM waitlist WHERE email = 'dave@example.com' AND confirmed IS NOT NULL")" = 0 ] \
  || { echo "dave was on the list before confirming"; exit 1; }
# opening the link (as a mail scanner would) only shows a button
curl -sf "$link" | grep -q 'value="[0-9a-f]\{64\}"' || { echo "no button behind dave's link"; exit 1; }
[ "$(d1get "SELECT count(*) FROM waitlist WHERE email = 'dave@example.com' AND confirmed IS NOT NULL")" = 0 ] \
  || { echo "opening dave's link confirmed it"; exit 1; }
press() { curl -s "http://localhost:$gport/waitlist/confirm" --data-urlencode "token=${link##*token=}"; }
press | grep -q "on the waitlist" || { echo "dave's button didn't work"; exit 1; }
[ "$(d1get "SELECT count(*) FROM waitlist WHERE email = 'dave@example.com' AND confirmed IS NOT NULL")" = 1 ] \
  || { echo "dave isn't on the waitlist"; exit 1; }
press | grep -q "expired" || { echo "dave's link worked twice"; exit 1; }
# the invitation, emailed by the cron trigger (alice and bob have accounts,
# so they get none)
d1 "INSERT INTO invites (email, created) VALUES ('carol@example.com', 1)"
curl -sf "http://localhost:$gport/__scheduled" >/dev/null
[ "$(d1get "SELECT count(*) FROM invites WHERE notified IS NULL")" = 0 ] || { echo "invitations not marked sent"; exit 1; }
grep -q "sign in with Google as carol@example.com" $(sent_to carol@example.com) /dev/null \
  || { echo "carol's invitation didn't go"; tail -30 "$work/server.log"; exit 1; }
[ -z "$(sent_to alice@example.com)" ] || { echo "alice was invited again"; exit 1; }
[ -n "$(sent_to admin@example.com)" ] || { echo "no digest for the admin"; exit 1; }
# the form's rate limit: five a minute, counted in windows aligned to the
# clock, so eleven at once are refused at least once even if a window turns
# over among them
codes=$(for n in $(seq 11); do ask "burst$n@example.com" human; echo; done)
grep -q 429 <<<"$codes" || { echo "no rate limit: $codes"; exit 1; }
echo "waitlist ok"
# the usage count, as the cron trigger runs it (it ran just now)
used=$(d1get "SELECT used FROM accounts WHERE id = '$a'")
listed=$(upstream GET /everyone "list-type=2&prefix=$a%2F" | grep -o '<Size>[0-9]*</Size>' | grep -o '[0-9]*' | paste -sd+ | python3 -c 'print(eval(input()))')
[ "$used" = "$listed" ] || { echo "usage $used, listed $listed"; exit 1; }
echo "usage count ok"
