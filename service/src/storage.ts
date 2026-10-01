// Object storage for the managed tiers. Devices never hold bucket
// credentials: they ask their account's Durable Object for short lived
// presigned URLs and move data straight to and from the bucket. Listing and
// deleting go through the service. Every path is a restic repository path,
// optionally under an epoch prefix, and lands under the account's own prefix,
// so a device can only ever touch its own account's objects, and a removed
// device loses access to storage at once.

import { AwsClient } from "aws4fetch";

/** How long a presigned URL stays valid. */
const EXPIRES_S = 900;

/** `config`, `keys/<id>`, `snapshots/<id>`, `index/<id>`, `data/<xx>/<id>`, each optionally under `e<n>/`. */
const PATH = /^(e[1-9][0-9]{0,5}\/)?(config|(keys|snapshots|index)\/[0-9a-f]{64}|data\/([0-9a-f]{2})\/\4[0-9a-f]{62})$/;
/** What may be listed: a type directory, or the config file itself. */
const PREFIX = /^(e[1-9][0-9]{0,5}\/)?(config|keys\/|snapshots\/|index\/|data\/)$/;

export const validPath = (p: string) => PATH.test(p);
/** The key epoch a repository path belongs to. */
export const epochOf = (p: string) => Number(p.match(/^e([1-9][0-9]*)\//)?.[1] ?? 0);
export const validPrefix = (p: string) => PREFIX.test(p);

export type StorageEnv = {
  /** Bytes an account may store; 0 or unset for no limit. */
  STORAGE_QUOTA_BYTES?: string;
  STORAGE_ENDPOINT?: string;
  STORAGE_BUCKET?: string;
  STORAGE_REGION?: string;
  STORAGE_KEY_ID?: string;
  STORAGE_SECRET?: string;
};

export class Storage {
  private aws: AwsClient;
  private base: string;

  constructor(
    env: StorageEnv,
    private account: string,
  ) {
    if (!env.STORAGE_ENDPOINT || !env.STORAGE_BUCKET || !env.STORAGE_KEY_ID || !env.STORAGE_SECRET) {
      throw new Error("storage is not configured");
    }
    this.aws = new AwsClient({
      accessKeyId: env.STORAGE_KEY_ID,
      secretAccessKey: env.STORAGE_SECRET,
      service: "s3",
      region: env.STORAGE_REGION ?? "auto",
    });
    this.base = `${env.STORAGE_ENDPOINT.replace(/\/$/, "")}/${env.STORAGE_BUCKET}`;
  }

  private key(path: string): string {
    return `accounts/${this.account}/${path}`;
  }

  private url(path: string): string {
    return `${this.base}/${this.key(path)}`;
  }

  /**
   * A presigned URL for one object. A PUT URL signs its Content-Length, so
   * the bucket refuses an upload of any other size: what the quota counts is
   * what gets stored.
   */
  async sign(method: "GET" | "PUT", path: string, size?: number): Promise<string> {
    const url = new URL(this.url(path));
    url.searchParams.set("X-Amz-Expires", String(EXPIRES_S));
    const headers = method === "PUT" ? { "content-length": String(size ?? 0) } : undefined;
    const signed = await this.aws.sign(url.toString(), {
      method,
      headers,
      aws: { signQuery: true, allHeaders: true },
    });
    return signed.url;
  }

  /** Every object under `prefix`, as repository paths with sizes. */
  async list(prefix: string): Promise<Array<{ path: string; size: number }>> {
    const strip = this.key("").length;
    const out: Array<{ path: string; size: number }> = [];
    let token: string | undefined;
    do {
      const url = new URL(this.base);
      url.searchParams.set("list-type", "2");
      url.searchParams.set("prefix", this.key(prefix));
      if (token) url.searchParams.set("continuation-token", token);
      const res = await this.aws.fetch(url.toString());
      if (!res.ok) throw new Error(`listing failed: ${res.status}`);
      const xml = await res.text();
      for (const [, body] of xml.matchAll(/<Contents>([\s\S]*?)<\/Contents>/g)) {
        const key = body.match(/<Key>([^<]*)<\/Key>/)?.[1];
        const size = Number(body.match(/<Size>(\d+)<\/Size>/)?.[1] ?? "0");
        if (key && !key.endsWith("/")) out.push({ path: unescapeXml(key).slice(strip), size });
      }
      token = xml.match(/<NextContinuationToken>([^<]*)<\/NextContinuationToken>/)?.[1];
    } while (token);
    return out;
  }

  /** Every object of the account, every epoch. */
  async listAll(): Promise<Array<{ path: string; size: number }>> {
    return this.list("");
  }

  /** Delete objects; missing ones count as deleted. */
  async remove(paths: string[]): Promise<void> {
    for (const path of paths) {
      const res = await this.aws.fetch(this.url(path), { method: "DELETE" });
      if (!res.ok && res.status !== 404) throw new Error(`deleting ${path} failed: ${res.status}`);
    }
  }
}

function unescapeXml(s: string): string {
  return s
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&apos;/g, "'")
    .replace(/&amp;/g, "&");
}
