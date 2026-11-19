#!/bin/sh
# chappe installer.
#
# Downloads the prebuilt binaries for your OS and architecture from GitHub Releases,
# verifies their SHA-256 checksum, and installs them onto your PATH. Linux and macOS.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ansromanov/chappe/main/install.sh | sh
#
# By default it installs `chappe` (the command agents run) and `chappe-host` (the daemon
# that wakes idle sessions of this machine). To run the service itself, add `chappe-bus`:
#   curl -fsSL .../install.sh | CHAPPE_COMPONENTS="chappe chappe-host chappe-bus" sh
#
# Environment overrides:
#   CHAPPE_VERSION       release tag to install (default: latest), e.g. v0.1.0
#   CHAPPE_COMPONENTS    binaries to install, space separated (default: "chappe chappe-host")
#   CHAPPE_INSTALL_DIR   directory to install into (default: auto-detected PATH dir)
#   CHAPPE_DOWNLOAD_BASE address to download from instead of GitHub (a mirror, a test)

set -eu

REPO="ansromanov/chappe"
VERSION="${CHAPPE_VERSION:-latest}"
COMPONENTS="${CHAPPE_COMPONENTS:-chappe chappe-host}"
INSTALL_DIR="${CHAPPE_INSTALL_DIR:-}"

# --- pretty output --------------------------------------------------------
if [ -t 2 ]; then
  BOLD="$(printf '\033[1m')"; RED="$(printf '\033[31m')"
  GREEN="$(printf '\033[32m')"; YELLOW="$(printf '\033[33m')"
  RESET="$(printf '\033[0m')"
else
  BOLD=""; RED=""; GREEN=""; YELLOW=""; RESET=""
fi

info()  { printf '%s\n' "${BOLD}==>${RESET} $*"; }
warn()  { printf '%s\n' "${YELLOW}warning:${RESET} $*" >&2; }
error() { printf '%s\n' "${RED}error:${RESET} $*" >&2; exit 1; }

# --- detect platform ------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)  os_part="linux" ;;
  Darwin) os_part="macos" ;;
  *) error "unsupported OS: $os (chappe runs on Linux and macOS)" ;;
esac

case "$arch" in
  x86_64 | amd64)  arch_part="x86_64" ;;
  aarch64 | arm64) arch_part="aarch64" ;;
  *) error "unsupported architecture: $arch" ;;
esac

for c in $COMPONENTS; do
  case "$c" in
    chappe | chappe-bus | chappe-host) ;;
    *) error "unknown component: $c (chappe, chappe-host or chappe-bus)" ;;
  esac
done

asset="chappe-${os_part}-${arch_part}"
archive="${asset}.tar.gz"

# --- download tool and addresses ------------------------------------------
if command -v curl >/dev/null 2>&1; then
  download() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  download() { wget -qO "$2" "$1"; }
else
  error "need either curl or wget to download"
fi

if [ -n "${CHAPPE_DOWNLOAD_BASE:-}" ]; then
  base="${CHAPPE_DOWNLOAD_BASE%/}"
elif [ "$VERSION" = "latest" ]; then
  base="https://github.com/${REPO}/releases/latest/download"
else
  base="https://github.com/${REPO}/releases/download/${VERSION}"
fi

# --- choose the install dir -----------------------------------------------
if [ -z "$INSTALL_DIR" ]; then
  : "${HOME:?HOME is unset; set CHAPPE_INSTALL_DIR to say where to install}"
  # A writable directory that is already on PATH.
  for _dir in "/usr/local/bin" "${HOME}/.local/bin"; do
    case ":${PATH}:" in
      *":${_dir}:"*)
        if [ -w "$_dir" ] || [ ! -e "$_dir" ]; then
          INSTALL_DIR="$_dir"
          break
        fi
        ;;
    esac
  done
fi
if [ -z "$INSTALL_DIR" ]; then
  INSTALL_DIR="${HOME}/.local/bin"
  warn "installing to ${INSTALL_DIR} (not on PATH; set CHAPPE_INSTALL_DIR to override)"
fi

# --- download and verify --------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

info "Downloading ${BOLD}${archive}${RESET} (${VERSION})"
download "${base}/${archive}" "${tmp}/${archive}" \
  || error "failed to download ${base}/${archive} (no release for ${os_part}-${arch_part}?)"
download "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" \
  || error "failed to download ${base}/SHA256SUMS"

info "Verifying checksum"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${tmp}/${archive}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "${tmp}/${archive}" | awk '{print $1}')"
else
  error "need sha256sum or shasum to verify the download"
fi
expected="$(awk -v a="$archive" '$2 == a || $2 == "*"a {print $1}' "${tmp}/SHA256SUMS")"
[ -n "$expected" ] || error "no checksum for ${archive} in SHA256SUMS"
[ "$expected" = "$actual" ] || error "checksum mismatch for ${archive}
  expected: ${expected}
  actual:   ${actual}"

tar -xzf "${tmp}/${archive}" -C "$tmp" || error "could not unpack ${archive}"

# --- install --------------------------------------------------------------
as_root=""
if ! mkdir -p "$INSTALL_DIR" 2>/dev/null || [ ! -w "$INSTALL_DIR" ]; then
  if command -v sudo >/dev/null 2>&1; then
    warn "${INSTALL_DIR} is not writable; using sudo"
    as_root="sudo"
    $as_root mkdir -p "$INSTALL_DIR"
  else
    error "${INSTALL_DIR} is not writable and sudo is unavailable; set CHAPPE_INSTALL_DIR to a writable directory"
  fi
fi

for c in $COMPONENTS; do
  [ -f "${tmp}/${asset}/${c}" ] || error "${c} is not in ${archive}"
  chmod +x "${tmp}/${asset}/${c}"
  $as_root cp "${tmp}/${asset}/${c}" "${INSTALL_DIR}/${c}"
  info "${GREEN}Installed${RESET} ${BOLD}${INSTALL_DIR}/${c}${RESET}"
done

# --- PATH hint ------------------------------------------------------------
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) warn "${INSTALL_DIR} is not on your PATH. Add it, e.g.:
    export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac

info "Set ${BOLD}CHAPPE_URL${RESET} and a token, then run ${BOLD}chappe doctor${RESET} to check the setup."
