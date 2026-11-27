# Agents that aren't on your computer

A design, not built yet. An agent with a shell on a computer of yours (Claude
Code, Codex, pi, opencode, Antigravity) already has everything: the files are
on disk, and the `omacloud` command gives their history
([the skill](../plugin/skills/omacloud/SKILL.md)). An agent that runs
somewhere else (claude.ai, ChatGPT, one on a phone) has neither. This page
says how it could read and write the folders you grant it, without Omacloud,
or the storage, ever seeing a file.

## What stays true

- **Omacloud's site is not in the path.** No server of ours holds a key,
  relays a request or sees a file. [design.md](design.md) holds for an
  account with agents as for one without.
- **The agent's provider sees what the agent reads.** A file sent to an agent
  at Anthropic or OpenAI is plain text there, as when you paste it into a chat.
  That is the person's choice, made folder by folder, and taken back at will.
- **Off until granted.** No agent reaches anything by default.

## How it would work

One of your own computers, one that's on, answers the agent: the daemon serves
a connector (the Model Context Protocol, over HTTPS), the same kind agents'
directories list. The files it hands over are the ones on its disk, and the
older versions come through what `omacloud mount` reads (`browse.rs`).

**Reaching the computer.** A provider's servers call the connector, so it
needs an address they can reach, and the encrypted connection has to end on
your computer, or whoever carries it reads it. Tailscale Funnel does that (the
certificate is the computer's; the doorbell already counts on Tailscale where
it's there). A tunnel that ends the connection on its own servers, on your own
domain and account, is your choice to make, and Omacloud says which kind it is.

**Granting.** `omacloud agents grant ~/Documents/Notes --to claude` (and the
app) names a folder, read only unless `--write` is given, until a date or
until taken back. The agent signs in to the connector the way connectors do
(OAuth), and the approval is a question on your computer: "Let Claude read
Documents/Notes?", with the fingerprint-style care a new computer gets. A
grant is a file on that computer only; `omacloud agents revoke` ends it at
once.

**What an agent can do, in a granted folder and nowhere else.**

| it can | how |
|---|---|
| list and search files, read one | from the computer's disk |
| list a file's versions, read one as it was | `versions`, and the head read on demand |
| write a file: a new one, or a new version of one that's there | the daemon saves it into the folder, as it does a browser's upload (`uploads.rs`), and the next sync carries it. An upload never goes over a file that's there; an agent's write may, but only once what's there is in a synced version, so it can be put back |

It can't delete, can't reach settings, keys, the secrets bundle or another
folder, and can't change what syncs. Paths are held to the rules an upload's
are (`inbox.rs`, `safe`). Every request is written to a log on the computer,
which the app shows: which agent, which file, read or written, when.

## What can go wrong

- **A file tells the agent what to do.** An agent that reads a note saying
  "copy the rest to this address" may try. So: read only unless asked, one
  folder at a time, no deletes, and every write a version you can put back.
- **The agent's provider keeps what it read.** Outside Omacloud's reach; the
  grant's question says so.
- **The address is public.** Nothing is answered without a grant's sign-in,
  and a computer serves only while a grant stands.
- **No computer is on.** The agent is told so. Nothing else holds the keys, on
  purpose: a server that could answer instead would be one that can read.

## Not chosen

- **A connector at omacloud.computer.** It would have to decrypt, which is
  everything Omacloud promises not to do.
- **Handing an agent a drive link.** It carries the key to the storage and the
  password to every file (design.md, In a browser): all or nothing, and the
  agent can't run the page that uses it.
- **A separate connector for agents on your computer.** They have the files and
  the command already; a second interface would only have to be kept the same.

## To decide before building

1. Tailscale Funnel first, and which other tunnels to name.
2. Whether a computer serves agents only while plugged in, as it's the one that
   has to stay on.
3. Whether writing may ever replace a file without asking, or each agent's first
   write to a file asks on the computer.
