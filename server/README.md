# omacloud.computer

A Cloudflare Worker serving Omacloud's public pages (home, privacy, terms,
`/llms.txt`, `/health`, and `/install.sh`, a redirect to the latest release's
installer), and Omacloud in a browser at `/files/`: a page of static files that reads
a person's storage from their browser. It holds nothing of anybody's: no account
depends on it. It keeps no logs (observability is off), sets no cookies and has
no database.

- `src/`: the Worker (`index.ts` the paths; `pages.ts` the pages).
- `cloudflare.config.ts`: the Worker's configuration, for `cf`.
- `build-files.sh`: puts the browser page (`../web`, and the reader from
  `../crates/omacloud-web` built to WebAssembly) in `public/files`, which is not
  committed. The Worker serves it with its own headers (`files` in `index.ts`).
- `deploy.sh` (`npm run deploy`): builds the page, type checks, then deploys
  with `cf`.
- `npm run check`: types and unit tests.
