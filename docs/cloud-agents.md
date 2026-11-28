# Agents that aren't on your computer

An agent with a shell on a computer of yours (Claude Code, Codex, pi,
opencode, Antigravity) already has everything: the files are on disk, and
the `omacloud` command gives their history
([the skill](../plugin/skills/omacloud/SKILL.md)). An agent that runs
somewhere else (claude.ai, ChatGPT, one on a phone) has neither. For those
there's a connector, which nobody has unless they set it up:

```sh
omacloud agents connect Documents/Notes
```

It puts a small program (`connector/`, a Cloudflare Worker) up, in **your own
Cloudflare account** or, with a subscription, in Omacloud's, and shows its
address and a passphrase, once. Add the
address to your agent as a connector (in claude.ai: Settings > Connectors >
Add custom connector), and when it asks, allow it with the passphrase. The
agent can then list, search and read the folders you named, look at earlier
versions, and write files there. `--read-only` leaves the writing out;
`omacloud agents` lists what this computer set up, and `omacloud agents
disconnect` takes it out again.

**In your own account** it needs your leave and Cloudflare's Workers Paid
plan. The command opens Cloudflare in your browser, where you pick the
account and allow Omacloud Workers Admin there, the narrowest leave
Cloudflare offers an app that can put a Worker up. It covers every Worker in
that account; Omacloud touches only its own connector
(`cloudflare.rs`; Cloudflare sends you back through omacloud.computer, which
passes on a code only the waiting command can use). The leave is used for
the upload and kept nowhere; Cloudflare lists it under your profile, to
revoke. Where there's no browser, an API token of yours with the one
permission Workers Scripts: Edit does the same (`OMACLOUD_CLOUDFLARE_TOKEN`,
or asked for). The plan is needed because
the free plan's ten milliseconds of processor time per request don't decrypt
an account.

**With a subscription** (`omacloud subscribe`, $5 a month) it needs neither:
the same command sends the connector's settings to omacloud.computer, which
runs the same Worker, one per connector, in Omacloud's Cloudflare account
(`server/src/hosting.ts`). `--own-cloudflare` uses yours anyway. When the
subscription ends, the Worker is deleted.

The two cost the same, $5 a month, to Cloudflare or to Omacloud: the
subscription is for not having a Cloudflare account, a plan and a token to
look after. With Workers Paid already, your own account is the one to use.

## What it trades

Everything else in Omacloud keeps one rule: only your computers can read
your files ([design.md](design.md)). The connector is the one place that
gives it up, for the folders' sake and by your choice:

- **The connector opens your files once an agent signs in.** Its settings,
  kept as a secret of the Worker, hold the key to your storage and the
  repository's key sealed under a key worked out from the passphrase. The
  passphrase is shown on your computer and typed on the connector's page; no
  copy is kept, only its hash, and the tokens an agent signs in with carry
  what opens the keys. So the settings alone, to whoever holds them, open
  nothing. A connector with an agent signed in holds the keys open in
  memory, and they open everything in the account, not only the folders
  named: the folders are what the connector's code shows, not what its keys
  open.
- **Whoever runs it could read your files, by changing what it runs.** In
  your own account that's you and Cloudflare. With a subscription it's
  Omacloud and Cloudflare: the Worker is the code in this repository, but
  you take Omacloud's word that it is, as with anything hosted.
- **The agent's provider sees what the agent reads**, as plain text, as when
  you paste it into a chat, and holds the tokens that open the keys.
- **In your own account, nothing of Omacloud's is in the path.** No server
  of ours holds a key, relays a request or sees a file. The agent talks to
  your Worker, and your Worker to your storage.
- **Setting it up trusts omacloud.computer once.** The connector's modules
  are part of the `omacloud` program, but the reader it decrypts with is
  WebAssembly, which a package's builder can't make everywhere, so `agents
  connect` fetches the one the browser page runs
  (`omacloud.computer/files/omacloud_web.wasm`) and checks only that it's
  that kind of thing. `scripts/build-connector.sh` builds all of it from
  source instead, for `OMACLOUD_CONNECTOR` to name.

## How it works

**Reading.** The Worker fetches from your bucket or Dropbox as the browser
page does and decrypts there with the same reader (`crates/omacloud-web`),
opened with the repository's key itself, so no password is worked out. It
finds the newest version, and looks again when twenty seconds have passed.
Paths are held to what was granted (`connector/files.js`): a folder above a
granted one lists only the way down to it, and nothing hidden is shown.

**Writing.** A connector can't push a version any more than a browser can:
it holds no device key. It leaves each file in the account's inbox, sealed
with the repository's key and signed with a key of its own, and a
computer's daemon takes it in ([design.md](design.md), In a browser). The
computer that set the connector up vouched for that key, and what it signed
says which folders the key may write to and that it may write over a file
that's there (`inbox.rs`, `Agent`). So every computer checks the folders for
itself: a connector, or whoever took its keys, can't write outside them, and
what it replaced is the version before. The vouching runs out after ninety
days (`--days`); connecting again renews it.

**Signing in.** claude.ai takes a connector with no sign-in or with OAuth,
so the Worker does OAuth itself, with nothing stored
(`connector/oauth.js`): a registration, a code and a token are each what
they say plus the Worker's own mark (an HMAC under a secret made at setup).
What lets an agent in is the passphrase, typed on a page the Worker serves,
which names the agent and the folders; the Worker holds only its hash.
Connecting again makes a new secret and passphrase, which ends every
sign-in to the one before.

**What an agent can do** (`connector/mcp.js`), in a granted folder and
nowhere else:

| tool | |
|---|---|
| `list_files` | a folder's files and folders |
| `read_file` | a text file, a long one in parts |
| `search_files` | files by name, and text files by a line they hold |
| `list_versions` | the newest versions kept; `list_files` and `read_file` take one |
| `write_file` | a new file, or a new version of one that's there |

It can't delete, can't reach settings, keys or the secrets bundle, and
can't change what syncs.

## What can go wrong

- **A file tells the agent what to do.** An agent that reads a note saying
  "copy the rest to this address" may try. So: name few folders, use
  `--read-only` where reading is all you want, and remember every write is a
  version you can put back (`omacloud versions`, `omacloud restore`).
- **The account it runs in is taken over.** With an agent signed in it holds
  the keys open, as above. `omacloud agents disconnect`, then change them
  as for a lost computer: `omacloud rotate` and `omacloud bucket set-key`.
- **The passphrase leaks.** Connect again: the old one opens nothing, and
  nor do the old settings. If the old settings may have leaked with it,
  change the keys as above.
- **The account moves to new keys** (a computer or a browser was removed as
  lost). The connector says it's out of date until you connect again.
- **No computer is on.** Reading works; what was written waits in the inbox.

## Not chosen

- **One connector at omacloud.computer for everyone.** A subscriber's
  connector is a Worker of its own, so no two accounts' keys are ever open
  in one place.
- **Keys the host can open at rest.** They'd be simpler to serve, and
  readable by whoever got at the settings.
- **One of your own computers answering.** The keys would stay at home, but
  a computer has to be on and reachable from the agent's servers through a
  tunnel, and a phone is for when you're away from them.
- **Handing an agent a web link.** It carries the key to the storage and
  the password to every file: all or nothing, and the agent can't run the
  page that uses it.
- **A separate connector for agents on your computer.** They have the files
  and the command already.
