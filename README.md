# Pine

Local HTTPS MITM proxy that taps Claude Code traffic into `~/.pine`.

## Installation

Pine is written in Rust. Prebuilt Linux and macOS binaries are published
continuously from `main` (see below); you can also build from source with
Cargo on either platform. This repository also includes npm publishing tools,
but those do not mean an npm package has been published.

### From source (Linux and macOS)

Install a current stable Rust toolchain from [rustup](https://rustup.rs/). On
Ubuntu/Debian, install the native build prerequisites:

```bash
sudo apt-get update
sudo apt-get install -y build-essential cmake pkg-config ca-certificates
```

Then build and install:

```bash
git clone https://github.com/Leanmcp-Community/pine.git
cd pine
cargo install --path . --locked
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
pine --version
```

For an existing checkout, just run `cargo install --path . --locked` again.
`./install.sh` performs the same Cargo installation and also records an install
telemetry event. `--locked` uses the dependency versions in `Cargo.lock`.

Install Claude Code separately, then run `pine` to launch it through the
proxy. Linux CI is configured to build and test on x86_64 and ARM64.

### One-line installer (Linux and macOS)

```bash
curl -fsSL https://raw.githubusercontent.com/Leanmcp-Community/pine/main/scripts/install.sh | bash
```

This installs to `~/.local/bin` without sudo. It downloads the latest release
binary for your OS/arch (Linux x64/ARM64, glibc 2.35+; macOS x64/ARM64) and
verifies its SHA-256 checksum; neither Rust nor Node.js is required. If no
binary has been published for your platform, it builds from source instead,
installing Rust if needed (native build prerequisites still apply on Linux).

Releases are cut continuously: every push to `main` publishes a new release
tagged `edge-<build number>-<commit>` (see `.github/workflows/release-edge.yml`),
so `--version latest` (the default) tracks the tip of `main` rather than a
hand-bumped semver number. Tagged `vX.Y.Z` releases (`release-linux.yml`) still
exist for anyone who wants to pin a specific stable version.

To build from source explicitly, pin a release, or change the install folder:

```bash
bash scripts/install.sh --source
bash scripts/install.sh --version v0.2.0 --prefix "$HOME/.local/bin"
bash scripts/install.sh --version edge-58-a1b2c3d4e5f6
```

**Updating**: re-run the installer, or run `pine update` — it downloads
the latest release for your platform, checks whether it's actually different
(comparing `--version`, which embeds the git commit), and replaces the running
binary in place if so. `pine update` refuses to touch a Homebrew-managed
install; use `brew upgrade` for those. Remove `~/.local/bin/pine` to
uninstall the default binary installation.

### Homebrew (macOS)

```bash
brew tap leanmcp-community/pine https://github.com/Leanmcp-Community/pine.git
brew install --HEAD leanmcp-community/pine/pine
```

This builds from source; Homebrew installs Rust as a build dependency.
See [Homebrew setup](docs/HOMEBREW.md) for cloud configuration and background services.

### npm, apt, Snap, and Windows

**No npm installation is advertised yet.** Maintainers can use the
[publishing guide](docs/PUBLISHING.md) to publish Linux x64 and ARM64 binaries
and an npm launcher. npm is a distribution channel: the application stays Rust,
and the small JavaScript launcher runs the packaged executable. npm installation
will require Node.js, while the standalone binary will not.

There is no configured apt repository or Snap package. A Windows `.exe` requires
a Windows build and platform validation; Linux binaries cannot be renamed to
`.exe`. Windows release packaging is not configured. See the publishing guide
for the release process and the distinction between npm packages and executables.

## Usage

Pine mints a local root CA, stands up an HTTPS MITM proxy, then launches a
target with its traffic routed through that proxy and logged to
`~/.pine/sessions/<id>/`.

```bash
# Tap the Claude Code CLI (default; env-var proxy)
pine                       # launches `claude` under the proxy
pine -- --help             # args after `--` are forwarded to claude

# Tap Windsurf (Electron GUI; Chromium --proxy-server flags)
pine windsurf

# Tap the Antigravity CLI (`agy`, Google's agent CLI)
pine antigravity           # or the alias: pine agy
pine agy --agy-bin /path/to/agy

# Tap the OpenAI Codex CLI
pine codex
pine codex -- --help

# Just run the proxy and wire up your own client
pine proxy
```

### Trusting the CA (required for Antigravity & Windsurf)

`agy` is a Go binary and Windsurf is Chromium-based; both validate TLS against
the **OS trust store**, not env-var CA bundles. Install the pine root CA
once:

```bash
pine ca trust       # add root CA to the login keychain (macOS)
pine ca untrust     # remove it again
```

The Claude Code launcher sets `NODE_EXTRA_CA_CERTS` for the child process.
On Linux, `pine ca trust` prints manual certificate setup instructions; it
does not automatically modify certificate stores.

### What gets tapped

Each target ships with a default host allow-list (everything else is
blind-tunneled, not decrypted). Override with `--hosts a.com,*.b.com`.

- **claude** → `api.anthropic.com`, `*.anthropic.com`
- **codex** → `api.openai.com`, `*.openai.com`, `chatgpt.com`
- **antigravity** → `cloudcode-pa.googleapis.com`, `aiplatform.googleapis.com`,
  `*.googleapis.com`, Google auth hosts, and `api.anthropic.com` (Antigravity
  serves Claude models through Google's Cloud Code backend).

> **Note:** the proxy decrypts HTTP/1.1 only. Google's Cloud Code REST/SSE
> endpoints work over HTTP/1.1, but any pure-gRPC (HTTP/2) calls are tunneled
> without decryption. Likewise, the Electron app `agy` can launch over CDP
> inherits the proxy env vars but Chromium ignores `HTTPS_PROXY` for its own
> network stack — the core LLM traffic from the `agy` process itself is what
> gets captured.

## Optional cloud uploads

See [cloud setup and deployment instructions](README_CLOUD.md) for running your
own server locally or remotely, configuring uploads, and installing the background service.
