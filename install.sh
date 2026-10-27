#!/bin/sh
# Install or upgrade ferry on the Remote.
#   curl -fsSL https://raw.githubusercontent.com/disberd/ferry/main/install.sh | sh
#   curl -fsSL … | sh -s -- v0.0.1      (one release, not the newest)
set -eu

repo=disberd/ferry
asset=ferry-linux-x86_64.tar.gz
if [ -n "${1:-}" ]; then
  base="https://github.com/$repo/releases/download/$1"
else
  base="https://github.com/$repo/releases/latest/download"
fi
bin="$HOME/.local/bin"
skill="$HOME/.agents/skills/ferry"
plugin="$HOME/.local/share/ferry/herdr-plugin"
lock="$HOME/.local/state/ferry/serve.lock"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# 1. Download the release and check its hash.
curl -fsSL -o "$tmp/$asset" "$base/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
(cd "$tmp" && grep " $asset\$" SHA256SUMS | sha256sum -c - >/dev/null)
tar -xzf "$tmp/$asset" -C "$tmp"

# 2. Put the binary in place with a rename, so a running ferry keeps its old file.
mkdir -p "$bin"
cp "$tmp/ferry" "$bin/.ferry.new"
chmod 755 "$bin/.ferry.new"
mv -f "$bin/.ferry.new" "$bin/ferry"
echo "installed $("$bin/ferry" --version) at $bin/ferry"

# 3. The agent skill. Link it for agents that do not read ~/.agents/skills.
mkdir -p "$skill"
cp "$tmp/SKILL.md" "$skill/SKILL.md"
for d in "$HOME/.claude/skills" "$HOME/.codex/skills"; do
  [ -d "$d" ] || continue
  [ "$(cd "$d" && pwd -P)" = "$(cd "$skill/.." && pwd -P)" ] && continue
  [ -e "$d/ferry" ] || ln -s "$skill" "$d/ferry"
done

# 4. The herdr plugin folder. The hook runs ./ferry, so the manifest needs no PATH.
mkdir -p "$plugin"
cp "$tmp/herdr-plugin.toml" "$plugin/herdr-plugin.toml"
ln -sfn "$bin/ferry" "$plugin/ferry"

# 5. Link the plugin into herdr.
if command -v herdr >/dev/null 2>&1; then
  herdr plugin link "$plugin"
else
  echo "herdr not found: run 'herdr plugin link $plugin' after you install herdr"
fi

# 6. Stop the running ferry serve. The Workstation connects again and starts the new binary.
if [ -f "$lock" ] && ! flock -n "$lock" true; then
  pid=$(cat "$lock")
  case "$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null)" in
    *"ferry serve"*) kill "$pid" && echo "stopped ferry serve (pid $pid)" ;;
  esac
fi
