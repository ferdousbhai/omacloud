// onecloud's web page: an account's devices and storage, cutting off a lost
// device, deleting the account. It signs in with the recovery code, which
// stays in this page: the root key derived from it signs each request, and
// the device list is verified against that root before it is shown.

import type { DeviceEntry } from "../src/proto";
import { members, revokeEntry, verifyChain } from "./chain";
import { type Signer, authorization, fingerprint, rootFromCode } from "./keys";

const enc = new TextEncoder();
const $ = <T extends HTMLElement>(sel: string) => document.querySelector<T>(sel)!;

class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message);
  }
}

/** A request to this account, signed by the root. */
async function api<T>(root: Signer, method: string, rest: string, body?: unknown): Promise<T> {
  const path = `/v1/accounts/${root.publicKey}/${rest}`;
  const bytes = body === undefined ? new Uint8Array() : enc.encode(JSON.stringify(body));
  const res = await fetch(path, {
    method,
    headers: {
      authorization: await authorization(root, method, path, bytes),
      ...(body === undefined ? {} : { "content-type": "application/json" }),
    },
    body: body === undefined ? undefined : bytes,
  });
  const data = (await res.json().catch(() => ({}))) as { error?: string; message?: string };
  if (!res.ok) throw new ApiError(res.status, data.error ?? "", data.message ?? data.error ?? `error ${res.status}`);
  return data as T;
}

function human(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return i === 0 ? `${bytes} B` : `${v.toFixed(1)} ${units[i]}`;
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Partial<HTMLElementTagNameMap[K]> & { class?: string } = {},
  ...children: (Node | string)[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  const { class: cls, ...rest } = props;
  Object.assign(node, rest);
  if (cls) node.className = cls;
  node.append(...children);
  return node;
}

function show(id: "signin" | "account") {
  $("#signin").hidden = id !== "signin";
  $("#account").hidden = id !== "account";
}

function notice(text: string, kind: "error" | "ok" = "error") {
  window.scrollTo({ top: 0 });
  const n = $("#notice");
  n.textContent = text;
  n.className = `notice ${kind}`;
  n.hidden = false;
}

/** Ask, in a dialog; resolves true on confirm. `typed`, if given, must be typed to confirm. */
function confirmDialog(title: string, body: string, action: string, typed?: string): Promise<boolean> {
  const dialog = $<HTMLDialogElement>("#confirm");
  $("#confirm-title").textContent = title;
  $("#confirm-body").textContent = body;
  const input = $<HTMLInputElement>("#confirm-typed");
  const label = $("#confirm-typed-label");
  const ok = $<HTMLButtonElement>("#confirm-ok");
  ok.textContent = action;
  label.hidden = input.hidden = typed === undefined;
  input.value = "";
  label.textContent = typed ? `Type ${typed} to confirm` : "";
  ok.disabled = typed !== undefined;
  input.oninput = () => (ok.disabled = input.value.trim() !== typed);
  // buttons close it themselves: a `method="dialog"` form counts as a form
  // submission, which the page's CSP (form-action 'none') refuses
  ok.onclick = () => dialog.close("ok");
  $("#confirm-cancel").onclick = () => dialog.close("cancel");
  dialog.returnValue = "";
  dialog.showModal();
  return new Promise((resolve) => {
    dialog.onclose = () => resolve(dialog.returnValue === "ok");
  });
}

let root: Signer | null = null;

async function signIn(code: string) {
  $("#notice").hidden = true;
  const signer = await rootFromCode(code);
  if (!signer) {
    notice("A recovery code has 28 letters and digits.");
    return;
  }
  try {
    await api(signer, "GET", "devices");
  } catch (e) {
    notice(e instanceof ApiError && (e.status === 403 || e.status === 404) ? "No account has this recovery code." : String(e));
    return;
  }
  root = signer;
  $<HTMLInputElement>("#code").value = "";
  show("account");
  await render();
}

async function render() {
  if (!root) return;
  const r = root;
  const [entries, usage, epochs, requests] = await Promise.all([
    api<DeviceEntry[]>(r, "GET", "devices"),
    api<{ used: number; quota: number }>(r, "GET", "storage/usage"),
    api<{ epoch: number }[]>(r, "GET", "epochs"),
    api<{ device: string; name: string }[]>(r, "GET", "requests"),
  ]);
  const why = await verifyChain(r.publicKey, entries);
  if (why) {
    notice(`The device list from the service doesn't verify against your account (${why}). Nothing was changed.`);
    return;
  }
  $("#account-fp").textContent = await fingerprint(r.publicKey);
  $("#account-id").textContent = r.publicKey;
  $("#epoch").textContent = String(epochs.at(-1)?.epoch ?? 0);
  $("#usage").textContent = usage.quota ? `${human(usage.used)} of ${human(usage.quota)}` : human(usage.used);
  const bar = $<HTMLProgressElement>("#usage-bar");
  bar.hidden = !usage.quota;
  bar.max = usage.quota || 1;
  bar.value = Math.min(usage.used, usage.quota);

  const m = members(entries);
  const list = $("#devices");
  list.replaceChildren();
  for (const [key, name] of m.valid) {
    const cut = el("button", { class: "danger", textContent: "Cut off…" });
    cut.onclick = () => cutOff(key, name);
    list.append(
      el("li", {}, el("div", {}, el("strong", { textContent: name }), el("code", { textContent: await fingerprint(key) })), cut),
    );
  }
  const removed = $("#removed");
  removed.replaceChildren();
  for (const [key, name] of m.revoked) {
    removed.append(el("li", {}, el("div", {}, el("strong", { textContent: name }), el("code", { textContent: await fingerprint(key) }))));
  }
  $("#removed-section").hidden = m.revoked.size === 0;
  const reqs = $("#requests");
  reqs.replaceChildren();
  for (const q of requests) {
    reqs.append(el("li", {}, el("div", {}, el("strong", { textContent: q.name }), el("code", { textContent: await fingerprint(q.device) }))));
  }
  $("#requests-section").hidden = requests.length === 0;
}

async function cutOff(device: string, name: string) {
  if (!root) return;
  const ok = await confirmDialog(
    `Cut off ${name}?`,
    `${name} stops syncing at once and can no longer reach your stored data. It still holds the key to what it ` +
      "already downloaded; to change that key, run `onecloud rotate` on another device.",
    "Cut off",
  );
  if (!ok) return;
  notice(`Cutting off ${name}…`, "ok");
  // the list may have moved since it was shown: build on the latest
  for (let attempt = 0; attempt < 3; attempt++) {
    const entries = await api<DeviceEntry[]>(root, "GET", "devices");
    const why = await verifyChain(root.publicKey, entries);
    if (why) {
      notice(`The device list doesn't verify (${why}); nothing was changed.`);
      return;
    }
    if (!members(entries).valid.has(device)) break;
    try {
      await api(root, "POST", "devices", await revokeEntry(root, entries, device));
      break;
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 409)) {
        notice(e instanceof Error ? e.message : String(e));
        return;
      }
    }
  }
  notice(`${name} is cut off.`, "ok");
  await render();
}

async function deleteAccount() {
  if (!root) return;
  const fp = await fingerprint(root.publicKey);
  const ok = await confirmDialog(
    "Delete this account?",
    "Every stored file, version and device goes, for good. Files on your devices stay where they are.",
    "Delete account",
    fp,
  );
  if (!ok) return;
  try {
    await api(root, "DELETE", "account");
  } catch (e) {
    notice(e instanceof Error ? e.message : String(e));
    return;
  }
  root = null;
  show("signin");
  notice("The account is deleted.", "ok");
}

$<HTMLFormElement>("#signin-form").onsubmit = (ev) => {
  ev.preventDefault();
  void signIn($<HTMLInputElement>("#code").value);
};
$("#signout").onclick = () => location.reload();
$("#refresh").onclick = () => void render().catch((e) => notice(String(e)));
$("#delete").onclick = () => void deleteAccount();
show("signin");
