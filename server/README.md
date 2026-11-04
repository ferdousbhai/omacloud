# omacloud.computer

A Cloudflare Worker serving Omacloud's public pages (home, privacy, terms,
`/llms.txt`, `/health`, and `/install.sh`, a redirect to the latest release's
installer). It holds nothing of anybody's: no account depends on it. It keeps
no logs (observability is off), sets no cookies and has no database.

- `src/`: the Worker (`index.ts` the paths; `pages.ts` the pages).
- `cloudflare.config.ts`: the Worker's configuration, for `cf`.
- `deploy.sh` (`npm run deploy`): type checks, then deploys with `cf`.
- `npm run check`: types and unit tests.
