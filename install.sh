#!/bin/sh
# Installs iph on a Mac from the latest GitHub release, with only the tools macOS
# ships (curl, tar, shasum). No Rust needed.
#
#   curl -fsSL https://raw.githubusercontent.com/HyunSeob/iphone-cli/main/install.sh | sh
#
# IPH_VERSION=v0.1.0 picks a release instead of the latest one.
# IPH_INSTALL_DIR picks the folder instead of ~/.local/bin.
set -eu

repo="HyunSeob/iphone-cli"
asset="iph-macos-universal.tar.gz"
dir="${IPH_INSTALL_DIR:-$HOME/.local/bin}"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "install.sh is for macOS. On Windows, download iph-windows-x86_64.zip from https://github.com/$repo/releases" >&2
  exit 1
fi

if [ -n "${IPH_VERSION:-}" ]; then
  base="https://github.com/$repo/releases/download/$IPH_VERSION"
else
  base="https://github.com/$repo/releases/latest/download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $base/$asset"
curl -fsSL -o "$tmp/$asset" "$base/$asset"
curl -fsSL -o "$tmp/$asset.sha256" "$base/$asset.sha256"
(cd "$tmp" && shasum -a 256 -c "$asset.sha256" >/dev/null) || {
  echo "Checksum mismatch for $asset" >&2
  exit 1
}

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
# Replace rather than overwrite in place: a running iph keeps its old file.
cp "$tmp/iph/iph" "$dir/iph.new"
chmod 755 "$dir/iph.new"
mv -f "$dir/iph.new" "$dir/iph"
xattr -d com.apple.quarantine "$dir/iph" 2>/dev/null || true

echo "Installed $("$dir/iph" --version) to $dir/iph"
case ":$PATH:" in
  *":$dir:"*) ;;
  *)
    echo
    echo "$dir is not on your PATH. Add it, for example:"
    echo "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc && exec zsh"
    ;;
esac
echo
echo "Next: plug the iPhone in by USB, tap Trust on it, and run: iph setup"
