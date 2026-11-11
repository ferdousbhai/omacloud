# omacloud.computer

A Cloudflare Worker serving Omacloud's public pages (home, privacy, terms,
`/llms.txt`, `/health`, and `/install.sh`, a redirect to the latest release's
installer), and Omacloud Drive at `/drive/`: a page of static files that reads
a person's storage from their browser. It holds nothing of anybody's: no account
depends on it. It keeps no logs (observability is off), sets no cookies and has
no database.

- `src/`: the Worker (`index.ts` the paths; `pages.ts` the pages).
- `cloudflare.config.ts`: the Worker's configuration, for `cf`.
- `build-drive.sh`: puts Omacloud Drive's page (`../web`, and the reader from
  `../crates/omacloud-web` built to WebAssembly) in `public/drive`, which is not
  committed. The Worker serves it with its own headers (`drive` in `index.ts`).
- `deploy.sh` (`npm run deploy`): builds the page, type checks, then deploys
  with `cf`.
- `npm run check`: types and unit tests.
