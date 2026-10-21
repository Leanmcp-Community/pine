#!/usr/bin/env bash
# Install a release binary (Linux or macOS), or build from source.
#
# Running this again is how you update: it always re-resolves `--version`
# (default: `latest`) and overwrites the binary at `--prefix`.
set -euo pipefail

# `pn` is a short alias: a symlink to `pine` in the same folder. Skipped if a
# different `pn` already exists there or earlier on PATH, so nothing is clobbered.
link_short_alias() {
    local dir="$1" existing
    if [ -e "$dir/pn" ] && [ ! -L "$dir/pn" ]; then
        echo ">>> Skipped the pn alias: $dir/pn already exists and isn't ours."
        return
    fi
    existing=$(command -v pn 2>/dev/null || true)
    if [ -n "$existing" ] && [ "$existing" != "$dir/pn" ]; then
        echo ">>> Skipped the pn alias: another pn is already on your PATH ($existing)."
        return
    fi
    ln -sf pine "$dir/pn"
    echo ">>> Alias: $dir/pn -> pine"
}

main() {
    local repo="Leanmcp-Community/pine"
    local version="latest" source_only=false
    local prefix="${PINE_INSTALL_DIR:-$HOME/.local/bin}"
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --source) source_only=true; shift ;;
            --version|--prefix)
                if [ "$#" -lt 2 ] || [ -z "$2" ]; then
                    echo "error: $1 requires a value" >&2; return 1
                fi
                case "$1" in
                    --version) version="$2" ;;
                    --prefix) prefix="$2" ;;
                esac
                shift 2 ;;
            --help|-h)
                echo "Usage: bash install.sh [--version TAG] [--prefix DIR] [--source]"
                echo "Installs to ~/.local/bin by default. --source builds with Cargo."
                echo "Re-run with no arguments any time to update to the latest release."
                return ;;
            *) echo "error: unknown option: $1" >&2; return 1 ;;
        esac
    done
    local os target
    case "$(uname -s)" in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        *) echo "error: this installer supports Linux and macOS; see README.md for other platforms" >&2; return 1 ;;
    esac
    local arch
    case "$(uname -m)" in
        x86_64|amd64) arch=x86_64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *) echo "error: supported architectures are x86_64 and ARM64" >&2; return 1 ;;
    esac
    if [ "$os" = linux ]; then
        target="${arch}-unknown-linux-gnu"
    else
        target="${arch}-apple-darwin"
    fi
    if [[ "$version" != latest && ! "$version" =~ ^(v[0-9]+\.[0-9]+\.[0-9]+([.-][a-zA-Z0-9.-]+)?|edge-[0-9]+-[0-9a-f]+)$ ]]; then
        echo "error: version must be latest, a release tag such as v0.2.1, or an edge-N-<hash> tag" >&2
        return 1
    fi
    local checksum_cmd
    if command -v sha256sum >/dev/null; then
        checksum_cmd=(sha256sum --check)
    elif command -v shasum >/dev/null; then
        checksum_cmd=(shasum -a 256 --check)
    else
        echo "error: sha256sum or shasum is required" >&2; return 1
    fi
    local command
    for command in curl tar install mktemp; do
        command -v "$command" >/dev/null || { echo "error: $command is required" >&2; return 1; }
    done
    local work
    work=$(mktemp -d)
    trap "$(printf 'rm -rf -- %q' "$work")" EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    local asset="pine-${target}.tar.gz"
    local base="https://github.com/$repo/releases"
    if [ "$version" = latest ]; then
        base="$base/latest/download"
    else
        base="$base/download/$version"
    fi
    if [ "$source_only" = false ]; then
        echo ">>> Downloading pine ($version, $target)"
        local status
        status=$(curl --proto '=https' --tlsv1.2 -sSL --retry 3 \
            -w '%{http_code}' -o "$work/$asset" "$base/$asset")
        if [ "$status" = 200 ]; then
            curl --proto '=https' --tlsv1.2 -fsSL --retry 3 \
                "$base/$asset.sha256" -o "$work/$asset.sha256"
            (cd "$work" && "${checksum_cmd[@]}" "$asset.sha256")
            tar -xzf "$work/$asset" -C "$work" pine
            if ! "$work/pine" --version; then
                echo "error: this binary won't run on this system; retry with --source" >&2
                return 1
            fi
        elif [ "$status" = 404 ]; then
            echo ">>> No release binary found; building from source instead."
            source_only=true
        else
            echo "error: release download returned HTTP $status" >&2
            return 1
        fi
    fi
    if [ "$source_only" = true ]; then
        if [ "$os" = linux ] && { ! command -v cc >/dev/null || ! command -v cmake >/dev/null; }; then
            echo "error: source builds require a C compiler and CMake." >&2
            echo "Ubuntu/Debian: sudo apt-get install -y build-essential cmake pkg-config ca-certificates" >&2
            return 1
        fi
        if [ "$os" = macos ] && ! command -v cc >/dev/null; then
            echo "error: source builds require a C compiler." >&2
            echo "macOS: xcode-select --install" >&2
            return 1
        fi
        if ! command -v cargo >/dev/null; then
            if [ -x "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" ]; then
                export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
            else
                echo ">>> Installing the stable Rust toolchain for the source build."
                curl --proto '=https' --tlsv1.2 -fsSL --retry 3 \
                    https://sh.rustup.rs -o "$work/rustup.sh"
                sh "$work/rustup.sh" -y --profile minimal --default-toolchain stable --no-modify-path
                export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
            fi
        fi
        local ref="refs/heads/main"
        [ "$version" = latest ] || ref="refs/tags/$version"
        curl --proto '=https' --tlsv1.2 -fsSL --retry 3 \
            "https://github.com/$repo/archive/$ref.tar.gz" -o "$work/source.tar.gz"
        mkdir "$work/source"
        tar -xzf "$work/source.tar.gz" -C "$work/source" --strip-components=1
        cargo install --path "$work/source" --locked --root "$work/built"
        cp "$work/built/bin/pine" "$work/pine"
    fi
    mkdir -p "$prefix"
    install -m 755 "$work/pine" "$prefix/pine"
    echo ">>> Installed: $prefix/pine"
    link_short_alias "$prefix"
    "$prefix/pine" --version
    case ":$PATH:" in
        *":$prefix:"*) ;;
        *) printf 'Add this to your shell profile, then restart your shell:\n  export PATH=%q:"$PATH"\n' "$prefix" ;;
    esac
}

main "$@"
