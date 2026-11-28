#!/usr/bin/env bash
# The connector for agents, end to end without Cloudflare: an account in a
# stand-in S3 (fake-s3.py), the settings `omacloud agents connect` makes for a
# connector, and the connector's own modules run by node against that
# storage. It has to list, read and search what it was given and nothing
# else, read an earlier version, and write a file that a computer's daemon
# takes in over the one that's there, and turns away outside its folders.
# Then where it goes: a person's own Cloudflare account (with a token, or
# with leave asked in a browser and returned through the site), or with a
# subscription Omacloud's, each stood in for.
#
#   scripts/connector.sh [path/to/omacloud]
#
# Needs node and Rust's wasm32-unknown-unknown target.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
OMACLOUD=$(realpath "${1:-target/debug/omacloud}")
work=$(mktemp -d)
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "${work:?}"
}
trap cleanup EXIT
fail() { echo "connector: $*" >&2; exit 1; }

scripts/build-connector.sh "$work/bundle"
echo key-one >"$work/keys"
python3 scripts/fake-s3.py "$work/port" "$work/keys" files &
pids+=($!)
for _ in $(seq 50); do [ -s "$work/port" ] && break; sleep 0.1; done
endpoint=http://127.0.0.1:$(cat "$work/port")

on_tty() { SHELL=/bin/bash script -qefc "$(printf '%q ' "$@")" /dev/null </dev/null | tr -d '\r'; }
oc() { "$OMACLOUD" --config "$work/a.toml" --styled "$@"; }
connect() { on_tty "$OMACLOUD" --config "$work/a.toml" agents connect "$@"; }
export OMACLOUD_SECRET_ACCESS_KEY=secret-one
mkdir -p "$work/A/Notes/Shared" "$work/A/Private"
echo "first draft" >"$work/A/Notes/Shared/plan.md"
printf 'alpha\nthe needle is here\nomega\n' >"$work/A/Notes/Shared/haystack.txt"
echo "mine alone" >"$work/A/Notes/diary.md"
echo "not for agents" >"$work/A/Private/secret.md"
on_tty "$OMACLOUD" --config "$work/a.toml" init --folder "$work/A" --provider s3 --endpoint "$endpoint" \
  --region us-east-1 --bucket files --access-key-id key-one --device a >/dev/null
oc sync >/dev/null 2>&1
echo "second draft" >"$work/A/Notes/Shared/plan.md"
oc sync >/dev/null 2>&1

# a connector is set up from a terminal, for a folder that's there
refused=$("$OMACLOUD" --config "$work/a.toml" agents connect Notes/Shared 2>&1 || true)
grep -q terminal_required <<<"$refused" || fail "the passphrase would print off a terminal: $refused"
export OMACLOUD_CONNECTOR=$work/bundle OMACLOUD_CONNECTOR_OUT=$work/settings.json
refused=$(connect Nowhere 2>&1 || true)
grep -q "isn't a folder here" <<<"$refused" || fail "a folder that isn't there was given: $refused"
shown=$(connect Notes/Shared)
grep -Eq '^ +([a-z0-9]{4}-){4}[a-z0-9]{4}$' <<<"$shown" || fail "no passphrase shown: $shown"
oc agents | grep -q "agents .* reads and writes Notes/Shared" || fail "the connector isn't listed: $(oc agents)"

# the settings alone open nothing: the keys in them are sealed under the
# passphrase, which isn't among them
sealed_only() {
	python3 -c 'import json, sys
settings = json.load(open(sys.argv[1]))
assert settings["sealed"] and not {"storage", "key", "upload", "passphrase"} & set(settings), "only sealed keys"' "$1"
}
sealed_only "$work/settings.json"
passphrase() { grep -Eo '([a-z0-9]{4}-){4}[a-z0-9]{4}' <<<"$1" | head -n 1; }
PASSPHRASE=$(passphrase "$shown")
export PASSPHRASE
if PASSPHRASE=aaaa-bbbb-cccc-dddd-eeee node scripts/connector/run.mjs "$work/bundle" "$work/settings.json" list_files >/dev/null 2>&1; then
	fail "another passphrase opened the connector's keys"
fi
call() { node scripts/connector/run.mjs "$work/bundle" "$work/settings.json" "$@"; }
[ "$(call list_files)" = "Notes/" ] || fail "the top lists more than the way to what was given: $(call list_files)"
[ "$(call list_files '{"path":"Notes"}')" = "Shared/" ] || fail "Notes shows what wasn't given: $(call list_files '{"path":"Notes"}')"
call list_files '{"path":"Notes/Shared"}' | grep -q '^plan.md ' || fail "the folder given isn't listed"
[ "$(call read_file '{"path":"Notes/Shared/plan.md"}')" = "second draft" ] || fail "the file reads wrong"
for outside in Notes/diary.md Private/secret.md ../a.toml Notes/Shared/../diary.md; do
  if call read_file "{\"path\":\"$outside\"}" >/dev/null; then fail "$outside was read"; fi
done
if call list_files '{"path":"Private"}' >/dev/null; then fail "Private was listed"; fi
call search_files '{"query":"NEEDLE"}' | grep -q '^Notes/Shared/haystack.txt:2: the needle is here$' || fail "search missed a line: $(call search_files '{"query":"NEEDLE"}')"
call search_files '{"query":"diary"}' | grep -q "Nothing found" || fail "search reached outside: $(call search_files '{"query":"diary"}')"
call search_files '{"query":"secret"}' | grep -q "Nothing found" || fail "search reached outside"

# an earlier version still reads
versions=$(call list_versions)
[ "$(wc -l <<<"$versions")" -ge 2 ] || fail "versions: $versions"
oldest=$(tail -n 1 <<<"$versions" | cut -d' ' -f1)
[ "$(call read_file "{\"path\":\"Notes/Shared/plan.md\",\"version\":\"$oldest\"}")" = "first draft" ] || fail "the first version reads wrong"

# what it writes, a computer takes in: over the file that's there, and new ones
"$OMACLOUD" --config "$work/a.toml" watch --poll 1 >"$work/watch.log" 2>&1 &
pids+=($!)
call write_file '{"path":"Notes/Shared/plan.md","content":"third draft, by an agent\n"}' | grep -q "^Sent Notes/Shared/plan.md" || fail "the write wasn't sent"
call write_file '{"path":"Notes/Shared/new/idea.md","content":"an idea\n"}' >/dev/null
if call write_file '{"path":"Notes/diary.md","content":"x"}' >/dev/null; then fail "a write outside the folder was sent"; fi
for _ in $(seq 150); do
  [ "$(cat "$work/A/Notes/Shared/plan.md")" = "third draft, by an agent" ] && [ -f "$work/A/Notes/Shared/new/idea.md" ] && break
  sleep 1
done
[ "$(cat "$work/A/Notes/Shared/plan.md")" = "third draft, by an agent" ] || { cat "$work/watch.log"; fail "the daemon didn't take the edit in"; }
[ "$(cat "$work/A/Notes/Shared/new/idea.md")" = "an idea" ] || fail "the new file didn't arrive"
[ ! -e "$work/A/Notes/Shared/plan (2).md" ] || fail "the edit landed beside the file, not over it"
[ "$(cat "$work/A/Notes/diary.md")" = "mine alone" ] || fail "a file outside the folder changed"
# and the version before is kept
for _ in $(seq 60); do call read_file '{"path":"Notes/Shared/plan.md"}' | grep -q "third draft" && break; sleep 1; done
call read_file '{"path":"Notes/Shared/plan.md"}' | grep -q "third draft" || fail "the edit never synced"
oc versions Notes/Shared/plan.md | grep -c . | grep -q '[3-9]' || fail "the versions before the edit are gone: $(oc versions Notes/Shared/plan.md)"

# a connector set up to read doesn't write
PASSPHRASE=$(passphrase "$(connect Notes/Shared --read-only)")
if call write_file '{"path":"Notes/Shared/plan.md","content":"x"}' >/dev/null; then fail "a read-only connector wrote"; fi
[ "$(call read_file '{"path":"Notes/Shared/plan.md"}')" = "third draft, by an agent" ] || fail "a read-only connector doesn't read"
unset OMACLOUD_CONNECTOR_OUT

# into a Cloudflare account, as a stand-in for its API sees it: the modules,
# the settings as a secret, its address turned on; and out again
python3 scripts/connector/fake-cloudflare.py "$work/cf-port" "$work/cf" &
pids+=($!)
for _ in $(seq 50); do [ -s "$work/cf-port" ] && break; sleep 0.1; done
OMACLOUD_CLOUDFLARE_API=http://127.0.0.1:$(cat "$work/cf-port")
export OMACLOUD_CLOUDFLARE_API
# as installed: the modules are omacloud's own, and the reader comes from
# where the browser page is served, looked at first
unset OMACLOUD_CONNECTOR
mkdir -p "$work/site/old"
cp "$work/bundle/omacloud_web.wasm" "$work/site/"
printf '\0asm not the reader' >"$work/site/old/omacloud_web.wasm"
(cd "$work/site" && exec python3 -u -m http.server 0 --bind 127.0.0.1 >"$work/site-port" 2>&1) &
pids+=($!)
for _ in $(seq 50); do grep -q "port [0-9]" "$work/site-port" 2>/dev/null && break; sleep 0.1; done
site=http://127.0.0.1:$(sed -n 's/.*port \([0-9]*\).*/\1/p' "$work/site-port" | head -n 1)
refused=$(OMACLOUD_CLOUDFLARE_TOKEN=token-one OMACLOUD_WEB_URL=$site/old/ connect Notes/Shared 2>&1 || true)
grep -q "isn't the reader" <<<"$refused" || fail "a reader that isn't one was taken: $refused"
[ ! -e "$work/cf/upload.json" ] || fail "something was uploaded without the reader"
export OMACLOUD_WEB_URL=$site/
refused=$(OMACLOUD_CLOUDFLARE_TOKEN=wrong connect Notes/Shared 2>&1 || true)
grep -q "Cloudflare refused" <<<"$refused" || fail "a wrong token wasn't said: $refused"
[ ! -e "$work/cf/upload.json" ] || fail "something was uploaded with a wrong token"
export OMACLOUD_CLOUDFLARE_TOKEN=token-one
shown=$(connect Notes/Shared --name notes)
grep -q "https://omacloud-notes.me.workers.dev/mcp" <<<"$shown" || fail "the connector's address isn't shown: $shown"
python3 - "$work/cf" "$work/bundle" <<'PY'
import json, os, sys
log, bundle = sys.argv[1], sys.argv[2]
upload = json.load(open(f"{log}/upload.json"))
assert upload["script"] == "omacloud-notes", upload
meta = json.load(open(f"{log}/metadata.json"))
assert meta["main_module"] == "worker.js" and meta["compatibility_date"], meta
[binding] = meta["bindings"]
assert binding["type"] == "secret_text" and binding["name"] == "OMACLOUD", binding
settings = json.loads(binding["text"])
assert settings["folders"] == ["Notes/Shared"] and settings["write"] is True, settings
assert settings["sealed"] and not {"storage", "key", "upload"} & set(settings), "the keys rest sealed"
assert len(binding["text"]) < 5000, "a secret is at most 5 KB"
# the passphrase itself is nowhere in what's uploaded
assert "passphrase" not in settings and len(settings["passphrase_hash"]) == 64
for name in os.listdir(bundle):
    part = upload["parts"][name]
    assert part["size"] == os.path.getsize(f"{bundle}/{name}"), name
    assert part["type"] == ("application/wasm" if name.endswith(".wasm") else "application/javascript+module"), part
assert set(upload["parts"]) == set(os.listdir(bundle)) | {"metadata"}
assert json.load(open(f"{log}/subdomain.json")) == {"enabled": True}
PY
oc agents | grep -q "^notes  https://omacloud-notes.me.workers.dev/mcp" || fail "the connector isn't listed: $(oc agents)"
oc agents disconnect notes >/dev/null
[ "$(cat "$work/cf/deleted")" = omacloud-notes ] || fail "the connector wasn't deleted"
if oc agents | grep -q "^notes "; then fail "a removed connector is still listed"; fi

# with a subscription, Omacloud runs the connector: a stand-in for the site
# sees settings whose keys are sealed, and nothing goes to Cloudflare
python3 scripts/connector/fake-site.py "$work/site2-port" "$work/paid-site" &
pids+=($!)
for _ in $(seq 50); do [ -s "$work/site2-port" ] && break; sleep 0.1; done
OMACLOUD_SITE=http://127.0.0.1:$(cat "$work/site2-port")
export OMACLOUD_SITE
# nobody who never ran `subscribe` has the site asked about them
oc subscription | grep -q "No subscription, and you may not need one" || fail "a subscription nobody started was looked up: $(oc subscription)"
oc subscribe | grep -Eq "^ +$OMACLOUD_SITE/subscribe/[0-9a-f]{64}$" || fail "no address to subscribe at: $(oc subscribe)"
oc subscription | grep -q "Not paid for yet" || fail "an unpaid subscription isn't said: $(oc subscription)"
# not paid for: it still goes to the person's own Cloudflare account
rm -f "$work/cf/upload.json"
connect Notes/Shared --name mine | grep -q "in your Cloudflare account" || fail "an unpaid subscription was used"
[ -e "$work/cf/upload.json" ] || fail "nothing went to the person's own Cloudflare account"
touch "$work/paid-site/paid"
rm -f "$work/cf/upload.json"
shown=$(connect Notes/Shared --name hosted)
grep -q "running at Omacloud" <<<"$shown" || fail "a paid subscription wasn't used: $shown"
grep -q "https://oc-test-hosted.omacloud.workers.dev/mcp" <<<"$shown" || fail "the hosted connector's address isn't shown: $shown"
[ ! -e "$work/cf/upload.json" ] || fail "a hosted connector also went to Cloudflare"
sealed_only "$work/paid-site/hosted-hosted.json"
# and those settings are a working connector, for the passphrase shown
PASSPHRASE=$(passphrase "$shown") node scripts/connector/run.mjs "$work/bundle" "$work/paid-site/hosted-hosted.json" list_files '{"path":"Notes/Shared"}' | grep -q '^plan.md ' \
	|| fail "the hosted connector's settings don't open the folder"
oc subscription | grep -q "^hosted  https://oc-test-hosted.omacloud.workers.dev/mcp" || fail "the subscription doesn't list its connector: $(oc subscription)"
oc subscription manage | grep -q "https://billing.stripe.com/p/test" || fail "no address to manage the subscription at"
connect Notes/Shared --name own --own-cloudflare | grep -q "in your Cloudflare account" || fail "--own-cloudflare went to Omacloud"
oc agents disconnect hosted >/dev/null
[ "$(cat "$work/paid-site/removed")" = hosted ] || fail "the hosted connector wasn't taken down"

# with no token, leave is asked of Cloudflare in a browser: the command shows
# where to go, the sign-in comes back through the site to the port it waits
# on, and the token that buys is what uploads
unset OMACLOUD_CLOUDFLARE_TOKEN
export OMACLOUD_CLOUDFLARE_OAUTH=$OMACLOUD_CLOUDFLARE_API/oauth2 OMACLOUD_CLOUDFLARE_CLIENT=client-one
rm -f "$work/cf/upload.json"
# (what it says is read as it's said: `script -f` writes each line down at once)
SHELL=/bin/bash script -qefc "$(printf '%q ' "$OMACLOUD" --config "$work/a.toml" agents connect Notes/Shared --name asked --own-cloudflare)" \
	"$work/asked.out" </dev/null >/dev/null 2>&1 &
asking=$!
pids+=("$asking")
for _ in $(seq 100); do grep -q "oauth2/auth?" "$work/asked.out" 2>/dev/null && break; sleep 0.1; done
address=$(tr -d '\r' <"$work/asked.out" | grep -o "http://127.0.0.1:[0-9]*/oauth2/auth?[^ ]*" | head -n 1 || true)
[ -n "$address" ] || fail "no address to allow Omacloud at: $(cat "$work/asked.out")"
grep -q "scope=workers-scripts.admin" <<<"$address" || fail "more than editing Workers was asked for: $address"
curl -fsSL "$address" | grep -q "Omacloud is allowed" || fail "the sign-in didn't come back to the command"
wait "$asking" || fail "the command didn't finish once allowed: $(cat "$work/asked.out")"
grep -q "https://omacloud-asked.me.workers.dev/mcp" "$work/asked.out" || fail "the connector isn't up after the sign-in: $(cat "$work/asked.out")"
[ -e "$work/cf/upload.json" ] || fail "nothing was uploaded with the sign-in's token"
echo "connector: passed"
