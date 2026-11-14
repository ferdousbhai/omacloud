# Omacloud files in a browser

A first slice of web access: a static page that reads an account's files
straight from the person's own S3 bucket and decrypts them in the browser.
No server of Omacloud's sees a file, a key or a request.

- `index.html`, `style.css`, `app.js`: the page, laid out as iCloud is on the
  web: a sign-in card, a home of tiles (Drive, Photos, Recents, Recently
  Deleted), then a bar on top, places and the account's folders on the left,
  the open folder as icons or a list (search, sort), and a look at a file over
  it (pictures, PDFs, text, audio and video) with a download. Photos and
  Recents walk every folder of the newest version once; Recently Deleted
  compares each kept version with the next, opening only the folders that
  differ.
- `s3.js`: listing and reading the bucket with presigned URLs, signed with the
  browser's own crypto.
- `reader.js`: calls `omacloud_web.wasm`, built from `crates/omacloud-web`,
  which decrypts what the page fetched.

It opens with a drive link (`omacloud drive-link`, or Open on a Phone in the
app): a computer seals the drive code, leaves it in the bucket under
`omacloud-links/` for five minutes, and shows a link, as a QR code too, to this
page with the seed and a presigned address of that object after the `#`. The
page takes the link out of its address at once, fetches the object, and opens it
with the seed (`link.rs` in the crate). The bucket holds only what it can't read,
and the link alone opens nothing once the object is gone. Or paste the code
`omacloud drive-code` prints (the bucket, its key and the repository password in
one line; what `omacloud export` prints works too), which only reads.

A session opened with a drive link can upload: the link carries a signing key
made for it and a member computer's endorsement of that key. The page signs
each file with the key, seals it with the repository's (`upload.rs` in the
crate), and puts it in `omacloud-inbox/` beside the repository; a computer's
daemon takes it in (`crates/omacloud-core/src/inbox.rs`,
`crates/omacloud/src/uploads.rs`). The page then watches for the version that
has it. Still to come: Dropbox, and putting a deleted file back.
The site's Worker serves it at omacloud.computer/drive/ (`server/build-drive.sh`
puts it in place at deploy).

The bucket must allow the page's origin to read it (a CORS rule allowing `GET`
from that origin, and `PUT` to upload); nothing else about the bucket changes.

## Try it

```sh
rustup target add wasm32-unknown-unknown   # once
scripts/web-files.sh                       # a throwaway account on a stand-in S3
```

It prints an address to open and what to paste, and runs until Ctrl-C.
