// Changing an account's storage key, as a bucket of your own lets you
// change its key at the provider: a computer asks for a new key and seals it
// to the account's other computers (`omacloud bucket set-key`); once every
// computer has switched, one of them retires the rest, and a removed
// computer's key with them.
//
// Both requests are signed like storage requests, with a key of the account.
// A removed computer that still holds a key could ask too, but every key it
// makes dies when the others retire it, and signing in with Google always
// gets a fresh key: at worst it makes the account's computers sign in again.
// An account holds at most MAX_KEYS: this API refuses past it, and a Google
// sign-in retires the oldest to make room (see db.newKey).

import * as db from "./db.ts";
import { authenticate } from "./gateway.ts";

/** POST /api/keys: a new key for the signer's account. */
export async function create(req: Request, env: Env): Promise<Response> {
	const url = new URL(req.url);
	const signer = await authenticate(req, env, url.pathname, url.search.slice(1));
	if (signer instanceof Response) return signer;
	const { account } = signer;
	if ((await db.activeKeys(env.DB, account.id)) >= db.MAX_KEYS)
		return Response.json({ error: "too many keys: change the key to retire old ones" }, { status: 429 });
	const key = await db.newKey(env.DB, account.id, db.masterKey(env));
	console.log(JSON.stringify({ event: "new key", account: account.id, key: key.accessKeyId, by: signer.accessKeyId }));
	return Response.json({ access_key_id: key.accessKeyId, secret_access_key: key.secret });
}

/** POST /api/keys/retire: revoke every key of the account but the one that signed. */
export async function retire(req: Request, env: Env): Promise<Response> {
	const url = new URL(req.url);
	const signer = await authenticate(req, env, url.pathname, url.search.slice(1));
	if (signer instanceof Response) return signer;
	const retired = await db.retireOthers(env.DB, signer.account.id, signer.accessKeyId);
	console.log(JSON.stringify({ event: "keys retired", account: signer.account.id, kept: signer.accessKeyId, retired }));
	return Response.json({ retired });
}
