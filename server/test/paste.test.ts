import assert from "node:assert/strict";
import { test } from "node:test";

// @ts-ignore: the Drive page's own script, plain JavaScript
import { pasted } from "../../web/paste.js";

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

test("anything else isn't taken for it", () => {
	assert.equal(pasted(""), null);
	assert.equal(pasted("hello"), null);
	// Dropbox's export goes through rclone, which the page can't use yet
	assert.equal(pasted("RESTIC_PASSWORD='x' restic -r rclone:dropbox:Apps/Omacloud restore latest"), null);
	assert.equal(pasted("restic -r s3:https://host.example restore latest"), null);
});
