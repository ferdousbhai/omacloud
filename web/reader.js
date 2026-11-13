// The decrypting half, compiled from crates/omacloud-web: bytes go in, the
// answer comes back as a length, a status byte and the content (see abi.rs).

const text = new TextEncoder();
const words = new TextDecoder();
let wasm;

export async function load(url) {
	wasm ??= (await WebAssembly.instantiateStreaming(fetch(url))).instance.exports;
}

function call(name, ...args) {
	const given = args.map((arg) => {
		const bytes = typeof arg === "string" ? text.encode(arg) : arg;
		const at = wasm.alloc(bytes.length);
		new Uint8Array(wasm.memory.buffer).set(bytes, at);
		return [at, bytes.length];
	});
	const answer = wasm[name](...given.flat());
	for (const [at, length] of given) wasm.dealloc(at, length);
	// memory may have grown during the call: look at it afresh
	const memory = new Uint8Array(wasm.memory.buffer);
	const length = new DataView(wasm.memory.buffer).getUint32(answer, true);
	const failed = memory[answer + 4];
	const content = memory.slice(answer + 5, answer + 5 + length);
	wasm.dealloc(answer, length + 5);
	if (failed) throw new Error(words.decode(content));
	return content;
}

const json = (bytes) => JSON.parse(words.decode(bytes));

// open with the repository's password and a key file; throws when it's wrong
export const open = (password, keyFile) => void call("open", password, keyFile);
// the JSON of a config or snapshot file, as stored
export const file = (stored) => json(call("file", stored));
// take in an index file; how many blobs it lists
export const addIndex = (stored) => Number(words.decode(call("add_index", stored)));
// where a blob is: { pack, offset, length }
export const locate = (id) => json(call("locate", id));
// a blob's content from its bytes in the pack, checked against its id
export const blob = (id, stored) => call("blob", id, stored);
// what a drive link's computer left in the bucket, opened with the link's seed
export const unseal = (seed, sealed) => json(call("unseal", seed, sealed));
