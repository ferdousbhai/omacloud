// Runs the connector's own modules (a built bundle) outside Cloudflare, for
// scripts/connector.sh: the settings `omacloud agents connect` made, the
// reader loaded from its file, and one tool called.
//
//   node run.mjs <bundle folder> <settings file> <tool> [arguments as JSON]
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const [bundle, settings, tool, args = "{}"] = process.argv.slice(2);
const from = (name) => import(pathToFileURL(`${bundle}/${name}`).href);
const { Files } = await from("files.js");
const { answer } = await from("mcp.js");
const config = JSON.parse(await readFile(settings, "utf8"));
const files = new Files(config, await WebAssembly.compile(await readFile(`${bundle}/omacloud_web.wasm`)));
const info = { version: config.version, folders: config.folders, write: config.write && !!config.upload };
const said = await answer({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: tool, arguments: JSON.parse(args) } }, files, info);
console.log(said.result.content[0].text);
process.exit(said.result.isError ? 1 : 0);
