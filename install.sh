#!/usr/bin/env bash
# claudetap installer
# Builds and installs the `claudetap` binary into ~/.cargo/bin via `cargo install`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found. Install Rust from https://rustup.rs/ first." >&2
    exit 1
fi

# A newer SDK can be installed alongside a linker that cannot read its stubs.
# Probe linking before Cargo starts so every build script uses a working SDK.
if [[ "$(uname -s)" == Darwin ]]; then
    PROBE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/claudetap-sdk.XXXXXX")"
    trap 'rm -rf "$PROBE_DIR"' EXIT
    sdk_links() {
        printf 'int main(void) { return 0; }\n' |
            "${CC:-cc}" -x c - -isysroot "$1" -o "$PROBE_DIR/probe" \
                >"$PROBE_DIR/link.log" 2>&1
    }
    SELECTED_SDK="${SDKROOT:-$(xcrun --sdk macosx --show-sdk-path)}"
    if ! sdk_links "$SELECTED_SDK"; then
        if [[ -n "${SDKROOT:-}" ]]; then
            cat "$PROBE_DIR/link.log" >&2
            echo "error: SDKROOT points to an SDK incompatible with the compiler. Unset SDKROOT to allow automatic selection." >&2
            exit 1
        fi
        SDK_FOUND=false
        for CANDIDATE_SDK in "$(dirname "$SELECTED_SDK")"/MacOSX*.sdk; do
            [[ -d "$CANDIDATE_SDK" ]] || continue
            if sdk_links "$CANDIDATE_SDK"; then
                SELECTED_SDK="$CANDIDATE_SDK"
                SDK_FOUND=true
                break
            fi
        done
        if [[ "$SDK_FOUND" != true ]]; then
            cat "$PROBE_DIR/link.log" >&2
            echo "error: no compatible macOS SDK found. Update the Xcode Command Line Tools and retry." >&2
            exit 1
        fi
        echo ">>> Using compatible macOS SDK: $SELECTED_SDK"
    fi
    export SDKROOT="$SELECTED_SDK"
fi

echo ">>> Installing claudetap from ${SCRIPT_DIR}"
cargo install --path "${SCRIPT_DIR}" --locked --color never "$@"

BIN_PATH="${CARGO_HOME:-$HOME/.cargo}/bin/claudetap"
if [[ -x "${BIN_PATH}" ]]; then
    # Send install event to PostHog
    "${BIN_PATH}" install-telemetry || true
    echo ">>> Installed: ${BIN_PATH}"
    case ":${PATH}:" in
        *":${CARGO_HOME:-$HOME/.cargo}/bin:"*) ;;
        *)
            echo ">>> Note: ${CARGO_HOME:-$HOME/.cargo}/bin is not in your PATH."
            echo "    Add this line to your shell profile:"
            echo "        export PATH=\"\$HOME/.cargo/bin:\$PATH\""
            ;;
    esac
else
    echo "warning: expected binary not found at ${BIN_PATH}" >&2
fi
