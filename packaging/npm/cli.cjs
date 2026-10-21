#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const manifest = require('../package.json');
const suffix = `-${process.platform}-${process.arch}`;
const name = Object.keys(manifest.optionalDependencies).find(key => key.endsWith(suffix));
if (!name) {
  console.error(`pine: no binary for ${process.platform}/${process.arch}. See the source installation instructions.`);
  process.exit(1);
}
let binary;
try {
  binary = require.resolve(`${name}/bin/pine`);
} catch {
  console.error(`pine: missing ${name}. Reinstall with optional dependencies enabled (npm install --include=optional).`);
  process.exit(1);
}
const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  console.error(`pine: cannot start the native binary: ${result.error.message}. Linux packages require glibc; see the source installation instructions.`);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
} else {
  process.exit(result.status ?? 1);
}
