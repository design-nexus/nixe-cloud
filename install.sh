#!/usr/bin/env bash
set -euo pipefail
# Piped from curl (`curl -fsSL …/install.sh | bash`): no checkout here, so fetch
# the source and run its installer from there.
if [[ -z ${BASH_SOURCE[0]:-} ]]; then
  command -v git >/dev/null || { echo "Git is needed to install nixe-cloud." >&2; exit 1; }
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  git clone -q --depth 1 https://github.com/design-nexus/nixe-cloud.git "$work/src"
  bash "$work/src/install.sh" "$@"
  exit
fi
cd -- "$(dirname -- "$0")"
# A release archive (.prebuilt) brings its binaries.
[[ -f .prebuilt ]] || cargo build --release --locked
install -Dm755 target/release/nixe-cloudd "$HOME/.local/bin/nixe-cloudd.new"
mv -f "$HOME/.local/bin/nixe-cloudd.new" "$HOME/.local/bin/nixe-cloudd"
install -Dm644 data/nixe-cloud.service "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/nixe-cloud.service"
install -Dm644 data/io.github.design_nexus.nixe.Cloud1.service "${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services/io.github.design_nexus.nixe.Cloud1.service"
systemctl --user daemon-reload
systemctl --user enable --now nixe-cloud.service
