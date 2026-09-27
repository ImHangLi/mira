#!/bin/sh
# Mira installer for macOS: user directory only, no sudo, no Node.
#   curl -fsSL https://raw.githubusercontent.com/ImHangLi/mira/main/scripts/install.sh | sh
# Environment:
#   MIRA_VERSION       release version to install (default: latest release)
#   MIRA_INSTALL_DIR   where to put the binary (default: ~/.mira/bin)
#   MIRA_INSTALL_FROM  local directory with release archives (offline/testing)
#   MIRA_NO_MODIFY_PATH  set to leave shell profiles unchanged (the PATH line is printed instead)
# The installer verifies the SHA-256 checksum, refuses to replace a `mira` it did not install,
# and never removes macOS quarantine attributes or bypasses Gatekeeper.
set -eu

repo="ImHangLi/mira"
dir="${MIRA_INSTALL_DIR:-$HOME/.mira/bin}"
marker="$dir/.mira-installed"

fail() { echo "mira install: $*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || fail "only macOS is supported"
[ "$(uname -m)" = "arm64" ] || fail "only Apple silicon (arm64) is supported"
target="aarch64-apple-darwin"

version="${MIRA_VERSION:-}"
version="${version#v}"
if [ -z "$version" ] && [ -z "${MIRA_INSTALL_FROM:-}" ]; then
  version="$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -1)"
  [ -n "$version" ] || fail "cannot find the latest release; set MIRA_VERSION"
fi
if [ -z "$version" ]; then
  archive="$(ls "$MIRA_INSTALL_FROM"/mira-*-"$target".tar.gz 2>/dev/null | tail -1)"
  [ -n "$archive" ] || fail "no archive for $target in $MIRA_INSTALL_FROM"
  name="$(basename "$archive" .tar.gz)"
else
  name="mira-$version-$target"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
if [ -n "${MIRA_INSTALL_FROM:-}" ]; then
  cp "$MIRA_INSTALL_FROM/$name.tar.gz" "$MIRA_INSTALL_FROM/$name.tar.gz.sha256" "$tmp/" || fail "archive not found"
else
  base="https://github.com/$repo/releases/download/v$version"
  curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz" || fail "download failed: mira $version has no $target build (see https://github.com/$repo/releases)"
  curl -fsSL "$base/$name.tar.gz.sha256" -o "$tmp/$name.tar.gz.sha256" || fail "checksum download failed"
fi
(cd "$tmp" && shasum -a 256 -c "$name.tar.gz.sha256" >/dev/null) || fail "checksum mismatch; nothing was installed"
tar -C "$tmp" -xzf "$tmp/$name.tar.gz"

if [ -e "$dir/mira" ] && [ ! -f "$marker" ]; then
  fail "$dir/mira exists and was not installed by this script; remove it or set MIRA_INSTALL_DIR"
fi
mkdir -p "$dir"
cp "$tmp/$name/mira" "$dir/mira.new"
chmod 0755 "$dir/mira.new"
mv "$dir/mira.new" "$dir/mira"
echo "$name" > "$marker"

other="$(command -v mira 2>/dev/null || true)"
echo "Installed $("$dir/mira" --version) to $dir/mira"
if [ -n "$other" ] && [ "$other" != "$dir/mira" ]; then
  echo "Note: another mira is first on PATH: $other"
fi
# Put the install directory on PATH for new shells, once, unless MIRA_NO_MODIFY_PATH is set.
# Check the shell profile, not this shell's PATH: an app that is already open (an editor, an
# agent) can still carry an old PATH entry that new terminals do not have.
case "$(basename "${SHELL:-}")" in
  # ~/.zshenv is read by every zsh, including the non-interactive shells agents run.
  zsh) rc="${ZDOTDIR:-$HOME}/.zshenv"; line="export PATH=\"$dir:\$PATH\"" ;;
  bash) rc="$HOME/.bash_profile"; line="export PATH=\"$dir:\$PATH\"" ;;
  fish) rc="$HOME/.config/fish/conf.d/mira.fish"; line="fish_add_path \"$dir\"" ;;
  *) rc="" ;;
esac
# The path is written into a shell profile, so it must not contain characters a shell
# would interpret there.
case "$dir" in
  *[\"\$\`\\]* | *"
"*) unsafe=1 ;;
  *) unsafe="" ;;
esac
if [ -n "${MIRA_NO_MODIFY_PATH:-}" ] || [ -z "$rc" ] || [ -n "$unsafe" ]; then
  echo "Add it to PATH:  export PATH=\"$dir:\$PATH\""
else
  # A line the user wrote with $HOME or ~ instead of the full path counts too.
  rel="${dir#"$HOME"}"
  if ! grep -qsF "$dir" "$rc" && ! grep -qsF "\$HOME$rel" "$rc" && ! grep -qsF "~$rel" "$rc"; then
    mkdir -p "$(dirname "$rc")"
    printf '\n# Added by the Mira installer\n%s\n' "$line" >> "$rc"
    echo "Added $dir to PATH in $rc."
  fi
  case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "Open a new terminal, or run now:  export PATH=\"$dir:\$PATH\"" ;;
  esac
fi
echo "Next: ask your agent to set up Mira. Agents start here: https://github.com/ImHangLi/mira/blob/main/docs/agents.md"
echo "Uninstall: rm \"$dir/mira\" \"$marker\", and remove the Mira line from your shell profile (project .mira files and workspace data are kept)."
