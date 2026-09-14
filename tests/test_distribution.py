"""Offline regression checks for installers and native npm packaging."""
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
VERSION = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]


class DistributionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.mocks = self.work / "mocks"
        self.mocks.mkdir()
        self.env = dict(os.environ, PATH=f"{self.mocks}:{os.environ['PATH']}",
                        FIXTURES=str(self.work), TMPDIR=str(self.work))

    def executable(self, path, text):
        path.write_text(text)
        path.chmod(0o755)

    def archive(self, arch, binary, version=VERSION, target=None):
        target = target or f"{arch}-unknown-linux-gnu"
        path = self.work / f"claudetap-{target}.tar.gz"
        with tarfile.open(path, "w:gz") as bundle:
            for name, data in {
                "claudetap": binary,
                "release.json": json.dumps({"version": version, "target": target}).encode(),
            }.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = 0o755 if name == "claudetap" else 0o644
                bundle.addfile(info, io.BytesIO(data))
        Path(f"{path}.sha256").write_text(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n")
        return path

    def run_command(self, *args):
        return subprocess.run(args, env=self.env, text=True, capture_output=True, cwd=ROOT)

    def setup_download(self):
        self.executable(self.mocks / "uname", '#!/bin/sh\nif [ "$1" = -s ]; then echo "${TEST_OS:-Linux}"; else echo "${TEST_ARCH:-x86_64}"; fi\n')
        self.executable(self.mocks / "curl", '''#!/bin/bash
set -eu
out=""; status=false; url=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -w) status=true; shift 2 ;;
    --proto|--tlsv1.2|--retry)
      if [ "$1" = --tlsv1.2 ]; then shift; else shift 2; fi ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
if [ "${HTTP_STATUS:-200}" != 200 ] && [ "$status" = true ]; then
  printf '%s' "$HTTP_STATUS"; exit 0
fi
cp "$FIXTURES/${url##*/}" "$out"
if [ "$status" = true ]; then printf 200; fi
''')

    def test_installer_binary_both_architectures_and_space_in_prefix(self):
        self.setup_download()
        for arch in ("x86_64", "aarch64"):
            self.env["TEST_ARCH"] = arch
            self.archive(arch, b'#!/bin/sh\necho "claudetap test"\n')
            prefix = self.work / f"install {arch}"
            result = self.run_command("bash", "scripts/install.sh", "--prefix", str(prefix))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((prefix / "claudetap").is_file())
        self.assertFalse(list(self.work.glob("tmp.*")), "installer temporary directory leaked")

    def test_installer_macos_uses_shasum_when_sha256sum_absent(self):
        self.setup_download()
        self.assertIsNotNone(shutil.which("shasum"), "test host needs shasum")
        trimmed_path = os.pathsep.join(
            d for d in os.environ["PATH"].split(os.pathsep)
            if not (Path(d) / "sha256sum").exists()
        )
        self.env["PATH"] = f"{self.mocks}{os.pathsep}{trimmed_path}"
        self.env["TEST_OS"] = "Darwin"
        for arch in ("x86_64", "aarch64"):
            self.env["TEST_ARCH"] = arch
            self.archive(arch, b'#!/bin/sh\necho "claudetap test"\n', target=f"{arch}-apple-darwin")
            prefix = self.work / f"install-macos-{arch}"
            result = self.run_command("bash", "scripts/install.sh", "--prefix", str(prefix))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((prefix / "claudetap").is_file())

    def test_bad_checksum_preserves_existing_install(self):
        self.setup_download()
        archive = self.archive("x86_64", b'#!/bin/sh\nexit 0\n')
        Path(f"{archive}.sha256").write_text(f"{'0' * 64}  {archive.name}\n")
        prefix = self.work / "installed"
        prefix.mkdir()
        existing = prefix / "claudetap"
        existing.write_text("existing installation")
        result = self.run_command("bash", "scripts/install.sh", "--prefix", str(prefix))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(existing.read_text(), "existing installation")

    def test_download_error_does_not_fall_back(self):
        self.setup_download()
        self.env["HTTP_STATUS"] = "403"
        result = self.run_command("bash", "scripts/install.sh")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("HTTP 403", result.stderr)
        self.assertNotIn("building from source", result.stdout)

    def test_404_builds_requested_source_tag(self):
        self.setup_download()
        self.env["HTTP_STATUS"] = "404"
        with tarfile.open(self.work / "v0.2.0.tar.gz", "w:gz") as bundle:
            info = tarfile.TarInfo("claudetap/Cargo.toml")
            info.size = 0
            bundle.addfile(info, io.BytesIO(b""))
        for tool in ("cc", "cmake"):
            self.executable(self.mocks / tool, "#!/bin/sh\nexit 0\n")
        self.executable(self.mocks / "cargo", '''#!/bin/bash
set -eu
while [ "$1" != --root ]; do shift; done
mkdir -p "$2/bin"
printf '#!/bin/sh\\necho claudetap-source\\n' > "$2/bin/claudetap"
chmod +x "$2/bin/claudetap"
''')
        prefix = self.work / "installed"
        result = self.run_command("bash", "scripts/install.sh", "--version", "v0.2.0", "--prefix", str(prefix))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("claudetap-source", result.stdout)

    def prepare(self, wrong_version=False):
        for arch, machine in (("x86_64", 62), ("aarch64", 183)):
            binary = bytearray(64)
            binary[:6] = b"\x7fELF\x02\x01"
            binary[18:20] = machine.to_bytes(2, "little")
            self.archive(arch, binary, "0.0.0" if wrong_version else VERSION)
        return self.run_command("python3", "scripts/prepare-npm.py", "--artifacts", str(self.work),
                                "--output", str(self.work / "npm"), "--name", "@test/claudetap")

    def test_npm_scoped_packages_and_launcher_exit_status(self):
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        wrapper = self.work / "npm/claudetap"
        manifest = json.loads((wrapper / "package.json").read_text())
        self.assertEqual(manifest["optionalDependencies"]["@test/claudetap-linux-arm64"], VERSION)
        cpu = self.run_command("node", "-p", "process.arch").stdout.strip()
        binary = wrapper / f"node_modules/@test/claudetap-linux-{cpu}/bin/claudetap"
        binary.parent.mkdir(parents=True)
        self.executable(binary, '#!/bin/sh\nprintf "%s\\n" "$@"\nexit 7\n')
        result = self.run_command("node", str(wrapper / "bin/claudetap.cjs"), "an argument", "--help")
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(result.stdout, "an argument\n--help\n")
        binary.unlink()
        result = self.run_command("node", str(wrapper / "bin/claudetap.cjs"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("optional dependencies", result.stderr)

    def test_npm_rejects_wrong_release_version(self):
        result = self.prepare(wrong_version=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("release metadata", result.stderr)
        self.assertFalse((self.work / "npm").exists())

    def test_publish_dry_run_and_dependency_order(self):
        self.assertEqual(self.prepare().returncode, 0)
        self.executable(self.mocks / "npm", '#!/bin/sh\nprintf "%s\\n" "$*" >> "$FIXTURES/npm.log"\n')
        result = self.run_command("node", "scripts/publish-npm.mjs", str(self.work / "npm"))
        self.assertEqual(result.returncode, 0, result.stderr)
        log = self.work / "npm.log"
        self.assertEqual(len(log.read_text().splitlines()), 3)
        self.assertTrue(all("--dry-run" in line for line in log.read_text().splitlines()))
        log.unlink()
        result = self.run_command("node", "scripts/publish-npm.mjs", str(self.work / "npm"), "--publish")
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = log.read_text().splitlines()
        self.assertEqual(len(lines), 6)
        self.assertTrue(all("--dry-run" in line for line in lines[:3]))
        self.assertTrue(all("--dry-run" not in line for line in lines[3:]))
        for line, directory in zip(lines[3:], ("linux-x64", "linux-arm64", "claudetap")):
            self.assertIn(f"/npm/{directory} ", line)

    def test_failed_preflight_never_publishes(self):
        self.assertEqual(self.prepare().returncode, 0)
        self.executable(self.mocks / "npm", '#!/bin/sh\nprintf "%s\\n" "$*" >> "$FIXTURES/npm.log"\nexit 1\n')
        result = self.run_command("node", "scripts/publish-npm.mjs", str(self.work / "npm"), "--publish")
        self.assertNotEqual(result.returncode, 0)
        lines = (self.work / "npm.log").read_text().splitlines()
        self.assertEqual(len(lines), 1)
        self.assertIn("--dry-run", lines[0])


if __name__ == "__main__":
    unittest.main()
