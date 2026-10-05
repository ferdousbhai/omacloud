import assert from "node:assert/strict";
import { test } from "node:test";

import { validPad } from "../src/signin.ts";

test("a pad is 28 characters of the recovery code's alphabet", () => {
	assert.ok(validPad("23456789abcdefghjkmnpqrstuvw"));
	assert.ok(!validPad("23456789abcdefghjkmnpqrstuv"));
	// no 0, 1, i, l or o, no upper case
	assert.ok(!validPad("03456789abcdefghjkmnpqrstuvw"));
	assert.ok(!validPad("i3456789abcdefghjkmnpqrstuvw"));
	assert.ok(!validPad("A3456789abcdefghjkmnpqrstuvw"));
	assert.ok(!validPad(null));
	assert.ok(!validPad(42));
});
