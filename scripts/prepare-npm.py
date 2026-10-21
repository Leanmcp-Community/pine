#!/usr/bin/env python3
"""Package existing, checksummed Linux release archives for npm; never publish."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = {"x64": ("x86_64", 62), "arm64": ("aarch64", 183)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, default=ROOT / "dist")
    parser.add_argument("--output", type=Path, default=ROOT / "dist/npm")
    parser.add_argument("--name", default="pine", help="npm name you own, optionally @scope/name")
    args = parser.parse_args()
    if not re.fullmatch(r"(?:@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*", args.name):
        parser.error("invalid npm package name")
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    # Validate every input before generating packages. Read only the expected binary.
    binaries = {}
    for cpu, (arch, machine) in PLATFORMS.items():
        archive = args.artifacts / f"pine-{arch}-unknown-linux-gnu.tar.gz"
        expected = Path(f"{archive}.sha256").read_text().split()[0]
        if hashlib.sha256(archive.read_bytes()).hexdigest() != expected:
            raise ValueError(f"checksum mismatch: {archive}")
        with tarfile.open(archive, "r:gz") as bundle:
            metadata = json.load(bundle.extractfile("release.json"))
            if metadata != {"version": version, "target": f"{arch}-unknown-linux-gnu"}:
                raise ValueError(f"release metadata does not match this checkout: {archive}")
            member = bundle.getmember("pine")
            if not member.isfile():
                raise ValueError(f"expected a regular binary file in {archive}")
            binary = bundle.extractfile(member).read()
        if (binary[:6] != b"\x7fELF\x02\x01" or
                int.from_bytes(binary[18:20], "little") != machine):
            raise ValueError(f"expected a 64-bit Linux {arch} ELF binary in {archive}")
        binaries[cpu] = binary
    if args.output.exists():
        raise ValueError(f"output already exists: {args.output}; choose a new --output directory")
    args.output.mkdir(parents=True)
    common = {
        "version": version,
        "license": "MIT",
        "repository": {"type": "git", "url": "git+https://github.com/Leanmcp-Community/pine.git"},
        "publishConfig": {"access": "public"},
        "os": ["linux"],
        "libc": ["glibc"],
    }
    dependencies = {}
    plan = []
    for cpu, binary in binaries.items():
        name = f"{args.name}-linux-{cpu}"
        dependencies[name] = version
        directory = args.output / f"linux-{cpu}"
        (directory / "bin").mkdir(parents=True)
        executable = directory / "bin/pine"
        executable.write_bytes(binary)
        executable.chmod(0o755)
        manifest = dict(common, name=name, cpu=[cpu], files=["bin"],
                        description=f"Pine native Rust binary for Linux {cpu} (glibc)")
        (directory / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")
        (directory / "README.md").write_text("Native Rust binary used by the Pine npm launcher. Requires glibc 2.35 or newer.\n")
        plan.append(directory.name)
    directory = args.output / "pine"
    (directory / "bin").mkdir(parents=True)
    launcher = directory / "bin/pine.cjs"
    launcher.write_bytes((ROOT / "packaging/npm/cli.cjs").read_bytes())
    launcher.chmod(0o755)
    manifest = dict(common, name=args.name, cpu=list(PLATFORMS),
                    description="Local HTTPS proxy that records AI coding agent traffic",
                    bin={"pine": "bin/pine.cjs", "pn": "bin/pine.cjs"}, files=["bin"],
                    engines={"node": ">=18"}, optionalDependencies=dependencies)
    (directory / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (directory / "README.md").write_text(
        "# Pine\n\nPine is written in Rust. This package launches its native Linux binary.\n\n"
        f"Install: `npm install -g {args.name}`. Run: `pine --help`.\n\n"
        "Supports Linux x64 and ARM64 with glibc 2.35 or newer. Optional dependencies must be enabled.\n"
        "For source installation and other platforms, see https://github.com/Leanmcp-Community/pine.\n")
    plan.append(directory.name)
    (args.output / "publish-plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    print(f"Prepared {len(plan)} npm packages at {args.output} (version {version}); nothing published.")


if __name__ == "__main__":
    main()
