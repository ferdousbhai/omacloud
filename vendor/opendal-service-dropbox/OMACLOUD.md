# Vendored opendal-service-dropbox

crates.io opendal-service-dropbox 0.58.2, patched so a refresh token works
without an app secret: Omacloud's own Dropbox app is a public client
(OAuth with PKCE), and no secret ships in the binary. An unset
`client_secret` is no longer an error, and the token refresh leaves the
parameter out when it's empty, as Dropbox refuses `client_secret=`.
The two changes are marked `Omacloud:` in `src/backend.rs` and `src/core.rs`.

Drop this copy when upstream refreshes without a secret.
