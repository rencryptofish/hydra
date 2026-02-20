#!/usr/bin/env bash
set -euo pipefail

HYDRA_REPO="${HYDRA_REPO:-git@github.com:rencryptofish/hydra.git}"
HYDRA_REF="${HYDRA_REF:-}"

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

if ! have_cmd cargo; then
  if ! have_cmd curl; then
    echo "error: curl is required to install Rust." >&2
    exit 1
  fi

  echo "Installing Rust toolchain (cargo)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal

  if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1090
    . "$HOME/.cargo/env"
  fi
fi

if ! have_cmd cargo; then
  echo "error: cargo is not on PATH after Rust install." >&2
  exit 1
fi

install_cmd=(cargo install --locked --force --git "$HYDRA_REPO")
if [ -n "$HYDRA_REF" ]; then
  install_cmd+=(--rev "$HYDRA_REF")
fi
install_cmd+=(hydra)

echo "Installing hydra from $HYDRA_REPO..."
"${install_cmd[@]}"

CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
if [ -x "$CARGO_BIN/hydra" ]; then
  case ":$PATH:" in
    *":$CARGO_BIN:"*) ;;
    *)
      echo "warning: $CARGO_BIN is not on PATH for this shell."
      echo "add this to your shell profile:"
      echo "  export PATH=\"$CARGO_BIN:\$PATH\""
      ;;
  esac
fi

if ! have_cmd tmux; then
  echo "warning: tmux is not installed."
  echo "hydra requires tmux at runtime. Install it with your package manager."
fi

echo "hydra install complete. Run: hydra"
