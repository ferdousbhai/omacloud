# Vendored rustic_backend

crates.io rustic_backend 0.7.0, with its OpenDAL service selection moved
from unconditional dependency features into `full-services`. One source
change, marked `Omacloud:` in `src/opendal.rs`: creating a repository on
Dropbox skips making the 256 `data/` subfolders, which Dropbox rate-limits
when made at once and makes anyway when a file is uploaded into them. Omacloud selects S3, filesystem and Dropbox through its own OpenDAL
dependency; upstream defaults remain available through `full-backends`.

Drop this copy when upstream allows callers to select storage services.
