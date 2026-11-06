/**
 * Refresh the vendored `@tiktools/plugin-ui` broker client from a TikTools
 * checkout (`TIKTOOLS_APP_DIR`, default `../TikTools-app`). The package is not
 * published to npm yet, so the plugin UIs build against this pinned copy.
 */
import { cp, readFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';

const root = resolve(import.meta.dir, '..');
const app = resolve(root, process.env.TIKTOOLS_APP_DIR ?? '../TikTools-app');
const source = join(app, 'packages', 'plugin-ui');
const target = join(root, 'vendor', 'tiktools-plugin-ui');
const pkg = JSON.parse(await readFile(join(source, 'package.json'), 'utf8')) as { version: string };
for (const file of ['package.json', 'README.md', 'src/broker.ts']) {
  await cp(join(source, file), join(target, file));
}
console.log(`Vendored @tiktools/plugin-ui ${pkg.version} from ${source}`);
