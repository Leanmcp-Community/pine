# macOS installation and uploader lifecycle

Pine is one Rust binary with two separate jobs:

1. `pine` launches the AI tool through the local proxy and writes captures to
   `~/.pine/sessions/`.
2. `pine cloud sync` reads those files, fetches server policy, uploads eligible
   chunks, and records acknowledgements in `~/.pine/cloud-state.json`.
   Failures retry without blocking local capture. It does not capture traffic by
   itself; use the proxy/launcher normally.

A remote server is separate. Mac clients do not need Python or Docker when using
a remote server. Docker is only needed for the documented local server setup.

## Install from the Homebrew tap

The source and formula live together in `Leanmcp-Community/pine` on branch
`main`. The current formula installs the development HEAD version from source.

```bash
brew tap leanmcp-community/pine https://github.com/leanmcp-community/pine.git
brew install --HEAD leanmcp-community/pine/pine
```

The explicit URL is necessary because Homebrew otherwise assumes a repository
named `homebrew-pine`. See [Homebrew taps](https://docs.brew.sh/Taps).

This initial formula builds from source and installs Rust as a build dependency.
For a fast public release, publish versioned Apple Silicon and Intel binaries or
Homebrew bottles, with checksums, and replace the HEAD-only formula with a
versioned release formula. HEAD is a development distribution path.

## One-time connection

```bash
export PINE_UPLOAD_KEY='your-key'
pine cloud configure --endpoint https://your-server
unset PINE_UPLOAD_KEY
brew services start leanmcp-community/pine/pine
```

Use `http://127.0.0.1:8080` for the local server. Do not run these commands with
sudo: the uploader must use your user account's captures and credentials.

## Manage automatic syncing

```bash
brew services info leanmcp-community/pine/pine
brew services stop leanmcp-community/pine/pine
brew services restart leanmcp-community/pine/pine
```

Inspect uploader logs with:

```bash
tail -f "$(brew --prefix)/var/log/pine-sync.log"
```

To see foreground progress, stop the brew service, then run `pine cloud sync`.
The service starts at user login; the server must be available for uploads.

## Migrate from the existing Cargo installation

From the source checkout, first remove the older custom service:

```bash
python3 deploy/client-service.py uninstall
```

Do not run the old service and the brew service together. Existing captures,
credentials and checkpoints are reused. If `which pine` still points to
`~/.cargo/bin/pine`, use the Homebrew binary explicitly:

```bash
"$(brew --prefix)/bin/pine" cloud status
```

After checking the new installation, you can remove the old Cargo binary with
`cargo uninstall pine`. This does not delete ~/.pine data.
