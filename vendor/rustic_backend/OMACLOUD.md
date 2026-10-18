# Vendored rustic_backend

crates.io rustic_backend 0.7.0, with its OpenDAL service selection moved
from unconditional dependency features into `full-services`. Omacloud
selects S3 and filesystem through its own OpenDAL dependency; upstream
defaults remain available through `full-backends`.

Drop this copy when upstream allows callers to select storage services.
