import assert from "node:assert/strict";
import { test } from "node:test";
import { op, queryPairs, rewriteDelete, unfolder, validKey } from "../src/gateway.ts";
import { canonicalQuery, canonicalRequest, parseAuth, sign } from "../src/sigv4.ts";
import { unescapeXml } from "../src/upstream.ts";

test("AWS's own SigV4 example: GET /test.txt with a range", async () => {
	const empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
	const canonical = canonicalRequest(
		"GET",
		"/test.txt",
		"",
		[
			["host", "examplebucket.s3.amazonaws.com"],
			["range", "bytes=0-9"],
			["x-amz-content-sha256", empty],
			["x-amz-date", "20130524T000000Z"],
		],
		empty,
	);
	assert.equal(
		await sign(
			"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
			"20130524T000000Z",
			"20130524/us-east-1/s3/aws4_request",
			canonical,
		),
		"f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
	);
});

test("authorization headers", () => {
	const a = parseAuth(
		"AWS4-HMAC-SHA256 Credential=AKID/20261005/omacloud/s3/aws4_request, " +
			"SignedHeaders=host;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
	);
	assert.equal(a?.accessKeyId, "AKID");
	assert.deepEqual(a?.signedHeaders, ["host", "x-amz-date"]);
	assert.equal(parseAuth("AWS4-HMAC-SHA256 Credential=a/b/c/d/e"), null);
});

test("queries are sorted and encoded", () => {
	assert.equal(canonicalQuery("prefix=a%2Fb/c&list-type=2&delete"), "delete=&list-type=2&prefix=a%2Fb%2Fc");
});

test("keys stay in the folder", () => {
	assert.ok(validKey("data/ab/abcdef"));
	assert.ok(validKey("a..b/.c"));
	for (const k of ["../u0/config", "data/../../x", "./x", "", "a\nb"]) assert.ok(!validKey(k), k);
});

test("operations", () => {
	const q = (s: string) => queryPairs(s)!;
	assert.equal(op("GET", "k", q("")), "Object");
	assert.equal(op("GET", null, q("list-type=2&prefix=data%2F")), "List");
	assert.equal(op("GET", null, q("")), null);
	assert.equal(op("GET", null, q("list-type=2&encoding-type=url")), null);
	assert.equal(op("POST", null, q("delete")), "DeleteMany");
	assert.equal(op("PUT", "k", q("acl")), null);
	assert.equal(op("PUT", null, q("policy")), null);
	assert.equal(op("PUT", "k", q("partNumber=1&uploadId=x")), "Multipart");
});

test("deletes move into the folder, and nothing else gets through", () => {
	const out = rewriteDelete(
		'<?xml version="1.0"?><Delete xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Object><Key>a&amp;b</Key></Object>\n<Object><Key>c</Key></Object><Quiet>true</Quiet></Delete>',
		"u1/",
	)!;
	assert.match(out, /<Key>u1\/a&amp;b<\/Key>/);
	assert.match(out, /<Key>u1\/c<\/Key>/);
	assert.match(out, /<Quiet>true<\/Quiet>/);
	for (const bad of [
		"<Delete><Object><Key>../u2/config</Key></Object></Delete>",
		"<Delete><Object><Key>a</Key><VersionId>v</VersionId></Object></Delete>",
		'<Delete xmlns:s="x"><s:Object><s:Key>a</s:Key></s:Object></Delete>',
		"<Delete><Object><Key><![CDATA[../x]]></Key></Object></Delete>",
		"<Delete><Object><Key>a&bogus;</Key></Object></Delete>",
		"<Delete><Object><Key>a</Key></Object>",
		"<Delete></Delete>",
	])
		assert.equal(rewriteDelete(bad, "u1/"), null, bad);
});

test("listings lose the folder", () => {
	const xml =
		"<ListBucketResult><Name>everyone</Name><Prefix>u1/data/</Prefix>" +
		"<Contents><Key>u1/data/x</Key><Size>3</Size></Contents></ListBucketResult>";
	assert.equal(
		unfolder(xml, "u1/", "everyone", "u1"),
		"<ListBucketResult><Name>u1</Name><Prefix>data/</Prefix>" +
			"<Contents><Key>data/x</Key><Size>3</Size></Contents></ListBucketResult>",
	);
});

test("XML entities", () => {
	assert.equal(unescapeXml("a&amp;b&#65;&#x42;"), "a&bAB");
	assert.equal(unescapeXml("a&b"), null);
	assert.equal(unescapeXml("&#x110000;"), null);
});
