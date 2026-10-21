#!/usr/bin/env node
// Validate all packages first; publish native dependencies before the launcher.
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const args = process.argv.slice(2);
const publish = args.includes('--publish');
const positional = args.filter(arg => arg !== '--publish');
if (positional.length > 1 || positional.some(arg => arg.startsWith('-'))) {
  console.error('Usage: node scripts/publish-npm.mjs [dist/npm] [--publish]');
  process.exit(1);
}
const root = resolve(positional[0] ?? 'dist/npm');
const plan = JSON.parse(readFileSync(resolve(root, 'publish-plan.json'), 'utf8'));
if (JSON.stringify(plan) !== JSON.stringify(['linux-x64', 'linux-arm64', 'pine'])) {
  throw new Error('Unexpected publish plan; regenerate with scripts/prepare-npm.py');
}
const packages = plan.map(directory => {
  const path = resolve(root, directory);
  const manifest = JSON.parse(readFileSync(resolve(path, 'package.json'), 'utf8'));
  return { path, manifest };
});
const launcher = packages.at(-1).manifest;
for (const { manifest } of packages.slice(0, -1)) {
  if (manifest.version !== launcher.version || launcher.optionalDependencies[manifest.name] !== manifest.version) {
    throw new Error('Native package versions must match the launcher');
  }
}
function run(path, dryRun) {
  const flags = ['publish', path, '--access', 'public', '--ignore-scripts', '--tag',
    launcher.version.includes('-') ? 'next' : 'latest'];
  if (dryRun) flags.push('--dry-run');
  const result = spawnSync('npm', flags, { stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
for (const { path } of packages) run(path, true);
if (publish) {
  for (const { path } of packages) run(path, false);
} else {
  console.log('Dry run complete; nothing published. Add --publish when ready.');
}
