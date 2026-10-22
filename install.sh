#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "$0")"
cargo build --release --locked
install -Dm755 target/release/nixe-cloudd "$HOME/.local/bin/nixe-cloudd.new"
mv -f "$HOME/.local/bin/nixe-cloudd.new" "$HOME/.local/bin/nixe-cloudd"
install -Dm644 data/nixe-cloud.service "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/nixe-cloud.service"
install -Dm644 data/io.github.design_nexus.nixe.Cloud1.service "${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services/io.github.design_nexus.nixe.Cloud1.service"
systemctl --user daemon-reload
systemctl --user enable nixe-cloud.service
