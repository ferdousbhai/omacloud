import assert from "node:assert/strict";
import { test } from "node:test";

// @ts-ignore: the Drive page's own script, plain JavaScript
import { account, linked, pasted } from "../../web/paste.js";

const printed = `# restore without omacloud: restic or rustic, this bucket and password
# (repository epoch 0)
AWS_ACCESS_KEY_ID='0123456789abcdef0123456789abcdef' AWS_SECRET_ACCESS_KEY='fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210' AWS_DEFAULT_REGION='auto' \\
  RESTIC_PASSWORD='omacloud-AAAA-BBBB-CCCC-DDDD' \\
  restic -r s3:https://0af9e0921b880657d84a6c07307f8aef.r2.cloudflarestorage.com/my-bucket/omacloud-e2 restore latest --target <dir>`;

const want = {
	endpoint: "https://0af9e0921b880657d84a6c07307f8aef.r2.cloudflarestorage.com",
	bucket: "my-bucket",
	root: "omacloud-e2",
	region: "auto",
	key: "0123456789abcdef0123456789abcdef",
	secret: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
	password: "omacloud-AAAA-BBBB-CCCC-DDDD",
};

test("what omacloud export prints is read, however a terminal wrapped the copy", () => {
	assert.deepEqual(pasted(printed), want);
	// a copy from a narrow terminal: a break every few characters, anywhere
	for (const width of [80, 61, 37, 20]) {
		const wrapped = printed
			.split("\n")
			.flatMap((line) => line.match(new RegExp(`.{1,${width}}`, "g")) ?? [""])
			.join("\n");
		assert.deepEqual(pasted(wrapped), want, `wrapped at ${width}`);
	}
	// all on one line, as some terminals copy it
	assert.deepEqual(pasted(printed.replaceAll("\\\n", " ")), want);
});

test("a drive code is read, whole or wrapped", () => {
	const code = `omacloud-drive-${Buffer.from(JSON.stringify({ v: 1, ...want })).toString("base64url")}`;
	assert.deepEqual(pasted(code), want);
	assert.deepEqual(pasted(`  ${code.match(/.{1,40}/g)!.join("\n")}\n`), want);
	// a damaged one isn't half read
	assert.equal(pasted(code.slice(0, 60)), null);
	assert.equal(pasted("omacloud-drive-e30"), null);
});

test("a drive link is read from after the #", () => {
	const b64 = (text: string | Uint8Array) => Buffer.from(text).toString("base64url");
	const seed = new Uint8Array(32).map((_, i) => i);
	const address = "https://account.r2.cloudflarestorage.com/files/omacloud-links/ab12?X-Amz-Credential=key%2F20261113%2Fauto&X-Amz-Signature=00ff";
	assert.deepEqual(linked(`#link=${b64(seed)}.${b64(address)}`), { seed, address });
	assert.equal(linked(`#link=${b64("http://127.0.0.1:9000/files/x")}`), null);
	assert.ok(linked(`#link=${b64(seed)}.${b64("http://127.0.0.1:9000/files/x")}`));
	// only a bucket over https, a whole seed, and nothing that isn't an address
	for (const bad of ["http://example.com/x", "javascript:alert(1)", "file:///etc/passwd", "not an address"]) {
		assert.equal(linked(`#link=${b64(seed)}.${b64(bad)}`), null, bad);
	}
	assert.equal(linked(`#link=${b64(seed.slice(1))}.${b64(address)}`), null);
	assert.equal(linked(""), null);
	assert.equal(linked("#section"), null);
});

test("what a drive link's code says may carry an upload key", () => {
	const said = { v: 1, ...want };
	assert.deepEqual(account(said), want);
	const seed = Buffer.alloc(32, 7).toString("base64url");
	const endorsement = { key: "aa", until: 5, device: "bb", sig: "cc" };
	const withKey = account({ ...said, upload: { inbox: "/omacloud-inbox/", seed, endorsement } });
	assert.equal(withKey?.upload?.inbox, "omacloud-inbox");
	assert.equal(withKey?.upload?.seed.length, 32);
	assert.deepEqual(JSON.parse(withKey!.upload!.endorsement), endorsement);
	// half of one is none
	assert.deepEqual(account({ ...said, upload: { inbox: "x", seed: "AAAA", endorsement } }), want);
	assert.deepEqual(account({ ...said, upload: { seed, endorsement } }), want);
});

test("anything else isn't taken for it", () => {
	assert.equal(pasted(""), null);
	assert.equal(pasted("hello"), null);
	// Dropbox's export goes through rclone, which the page can't use yet
	assert.equal(pasted("RESTIC_PASSWORD='x' restic -r rclone:dropbox:Apps/Omacloud restore latest"), null);
	assert.equal(pasted("restic -r s3:https://host.example restore latest"), null);
});
