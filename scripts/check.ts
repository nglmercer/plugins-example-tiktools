/**
 * Repository-level checks that need no TikTools checkout: every package has
 * a well-formed manifest with a unique id, and ids/versions keep the shapes
 * TikTools accepts. Full manifest validation runs in TikTools itself
 * (`tiktools-plugin validate`) or through the host loader.
 */
import { readdir, readFile, stat } from 'node:fs/promises';
import { join, resolve } from 'node:path';

const root = resolve(import.meta.dir, '..');
const ID = /^[a-z0-9][a-z0-9._-]{0,63}$/;
const SEMVER = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
const RUNTIMES = new Set(['native', 'process', 'wasm', 'napi-vm', 'declarative']);

const seen = new Map<string, string>();
const problems: string[] = [];
for (const entry of await readdir(join(root, 'plugins'), { withFileTypes: true })) {
  if (!entry.isDirectory()) continue;
  const manifestPath = join(root, 'plugins', entry.name, 'plugin.json');
  if (!(await stat(manifestPath).catch(() => null))) {
    problems.push(`plugins/${entry.name} has no plugin.json`);
    continue;
  }
  const manifest = JSON.parse(await readFile(manifestPath, 'utf8')) as Record<string, unknown>;
  const id = String(manifest.id ?? '');
  if (!ID.test(id)) problems.push(`plugins/${entry.name}: invalid id ${JSON.stringify(id)}`);
  if (!SEMVER.test(String(manifest.version ?? ''))) problems.push(`plugins/${entry.name}: invalid version`);
  if (!RUNTIMES.has(String(manifest.runtime ?? ''))) problems.push(`plugins/${entry.name}: unknown runtime`);
  const previous = seen.get(id);
  if (previous) problems.push(`plugins/${entry.name}: id ${id} already used by plugins/${previous}`);
  seen.set(id, entry.name);
}
if (problems.length) {
  console.error(problems.join('\n'));
  process.exit(1);
}
console.log(`${seen.size} plugin manifests OK: ${[...seen.keys()].sort().join(', ')}`);
