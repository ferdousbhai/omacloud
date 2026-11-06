# Vendored opendal-service-dropbox

crates.io opendal-service-dropbox 0.58.2. Omacloud coordinates through its create-only
writes, so this copy stays until upstream has them and the rest. Patched so a refresh
token works without an app secret: Omacloud's own Dropbox app is a public client
(OAuth with PKCE), and no secret ships in the binary. An unset
`client_secret` is no longer an error, and the token refresh leaves the
parameter out when it's empty, as Dropbox refuses `client_secret=`.
Also: create-only writes (Dropbox's add mode, with only a file at the path as a
conflict); a refused refresh token is `PermissionDenied`, not a JSON error; a token service
having a moment is a temporary error; a 503 is `RateLimited`; and access tokens are shared within a process. The changes are marked
`Omacloud:` in `src/backend.rs` and `src/core.rs`.

