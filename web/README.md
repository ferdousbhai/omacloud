# Omacloud files in a browser

A first slice of web access: a static page that reads an account's files
straight from the person's own S3 bucket and decrypts them in the browser.
No server of Omacloud's sees a file, a key or a request.

- `index.html`, `style.css`, `app.js`: the page, laid out as iCloud Drive is on
  the web: a sign-in card, then a bar on top, the account's folders on the left,
  the open folder as icons or a list (search, sort), and a look at a file over
  it (pictures, PDFs, text, audio and video) with a download.
- `s3.js`: listing and reading the bucket with presigned URLs, signed with the
  browser's own crypto.
- `reader.js`: calls `omacloud_web.wasm`, built from `crates/omacloud-web`,
  which decrypts what the page fetched.

Today it opens with what `omacloud export` prints, pasted in (the bucket, its key
and the repository password), read-only. Still to come: a browser approved as
a device instead of the password, Dropbox, uploading and Recently Deleted.
The site's Worker serves it at omacloud.computer/drive/ (`server/build-drive.sh`
puts it in place at deploy).

The bucket must allow the page's origin to read it (a CORS rule allowing `GET`
from that origin); nothing else about the bucket changes.

## Try it

```sh
rustup target add wasm32-unknown-unknown   # once
scripts/web-files.sh                       # a throwaway account on a stand-in S3
```

It prints an address to open and what to paste, and runs until Ctrl-C.
