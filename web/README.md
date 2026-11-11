# Omacloud files in a browser

A first slice of web access: a static page that reads an account's files
straight from the person's own S3 bucket and decrypts them in the browser.
No server of Omacloud's sees a file, a key or a request.

- `index.html`, `style.css`, `app.js`: the page: a form, then folders to open
  and files to download.
- `s3.js`: listing and reading the bucket with presigned URLs, signed with the
  browser's own crypto.
- `reader.js`: calls `omacloud_web.wasm`, built from `crates/omacloud-web`,
  which decrypts what the page fetched.

Today it opens with what `omacloud export` prints (the bucket, its key and the
repository password), read-only. Still to come: a browser approved as a device
instead of the password, Dropbox, previews, and serving it from
omacloud.computer.

The bucket must allow the page's origin to read it (a CORS rule allowing `GET`
from that origin); nothing else about the bucket changes.

## Try it

```sh
rustup target add wasm32-unknown-unknown   # once
scripts/web-files.sh                       # a throwaway account on a stand-in S3
```

It prints an address to open and what to enter, and runs until Ctrl-C.
