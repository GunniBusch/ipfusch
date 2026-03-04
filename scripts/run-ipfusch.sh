#!/usr/bin/env bash
set -euo pipefail

REPO="${IPFUSCH_REPO:-GunniBusch/ipfusch}"
VERSION="${IPFUSCH_VERSION:-}"

if [[ -z "${VERSION}" ]]; then
  VERSION="$(
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
      | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
      | head -n 1
  )"
fi

if [[ -z "${VERSION}" ]]; then
  echo "could not resolve release version for ${REPO}" >&2
  exit 1
fi

uname_s="$(uname -s)"
uname_m="$(uname -m)"

case "${uname_s}" in
  Linux)
    os="unknown-linux-gnu"
    ;;
  Darwin)
    os="apple-darwin"
    ;;
  *)
    echo "unsupported OS: ${uname_s}" >&2
    exit 1
    ;;
esac

case "${uname_m}" in
  x86_64|amd64)
    arch="x86_64"
    ;;
  arm64|aarch64)
    arch="aarch64"
    ;;
  *)
    echo "unsupported architecture: ${uname_m}" >&2
    exit 1
    ;;
esac

target="${arch}-${os}"
asset="ipfusch-${VERSION}-${target}.tar.gz"
url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"

workdir="$(mktemp -d)"
cleanup() {
  rm -rf "${workdir}"
}
trap cleanup EXIT

echo "downloading ${asset} from ${REPO}@${VERSION}" >&2
curl -fL "${url}" -o "${workdir}/${asset}"
tar -C "${workdir}" -xzf "${workdir}/${asset}"
chmod +x "${workdir}/ipfusch"

exec "${workdir}/ipfusch" "$@"
