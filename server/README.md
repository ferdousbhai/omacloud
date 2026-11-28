# omacloud.computer

A Cloudflare Worker: Omacloud in a browser at `/files/` (where `/` sends), a
page of static files that reads a person's storage from their browser; the
public pages (privacy, terms, `/subscribed`, `/llms.txt`, `/health`,
`/logo.svg`, and `/install.sh`, a redirect to the latest release's
installer); `/cloudflare/callback`, where Cloudflare returns someone who
allowed Omacloud in their account (`omacloud agents connect`), which only
sends them on to the command waiting on `127.0.0.1` and keeps nothing; and what it does for accounts
that ask. It keeps no logs (observability is off) and sets no cookies. For
someone who asked for nothing it holds nothing, and no account depends on it.

`src/paid.ts` is the one place it keeps anything, in D1. Free, for anyone
who chooses it in the page: a passkey's public key with what it sealed, under
the hash of a key their computer made. For subscribers: Stripe's ids and
standing, and the names of their connectors for agents. It runs each subscriber's connector as a Worker of its own in
this Cloudflare account (`src/hosting.ts`), with settings whose keys are
sealed under a passphrase it never sees. It needs, as secrets of the Worker:

| secret | what |
|---|---|
| `STRIPE_SECRET_KEY` | Stripe's API key, for checkout and the billing portal |
| `STRIPE_WEBHOOK_SECRET` | the signing secret of the webhook at `/api/stripe` (events `checkout.session.completed`, `customer.subscription.updated`, `customer.subscription.deleted`) |
| `PASSKEY_SECRET` | any long random string: signs passkey challenges |
| `CLOUDFLARE_API_TOKEN` | a token with Workers Scripts: Edit on the account below |
| `CLOUDFLARE_ACCOUNT` | the id of the account the connectors run in, which needs Workers Paid |

`./set-secret.sh NAME` sets one, asking for it with no echo. The monthly price's id is in `cloudflare.config.ts` (`STRIPE_PRICE`). Until
all of them and the database are there, `/api/` and `/subscribe/` answer
404 and the rest of the site works as it did.

- `src/`: the Worker (`index.ts` the paths; `pages.ts` the pages; `paid.ts`
  what it keeps for accounts, and `hosting.ts` the connectors it runs for
  subscribers).
- `cloudflare.config.ts`: the Worker's configuration, for `cf`.
- `build-files.sh`: puts the browser page (`../web`, and the reader from
  `../crates/omacloud-web` built to WebAssembly) in `public/files`, and the
  connector for agents (`../connector`) in `public/connector`, which the Worker
  uploads for subscribers and serves to nobody. Neither is committed. The Worker serves it with its own headers (`files` in `index.ts`).
- `deploy.sh` (`npm run deploy`): builds the page, type checks, then deploys
  with `cf`.
- `npm run check`: types and unit tests.
