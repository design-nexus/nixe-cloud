# Cloud

Shared, user-scoped rclone storage for Files and Settings. This repository builds
`nixe-cloudd` and the public `nixe-cloud` Rust client library independently.
Requires Rust, rclone (tested with 1.75.2), FUSE3, a session bus, and systemd user
services. Normal operations require no root privileges.

```
cargo build --release --locked
./install.sh
```

Installation enables the service for subsequent logins and installs D-Bus
activation. It does not start or restart applications. Settings → Cloud Storage
starts the service on demand. New accounts default to mounting when selected in
Files; mount-on-login is opt-in per account.

## Storage and ownership

New accounts use `$XDG_CONFIG_HOME/nixe/cloud/rclone.conf` (0600). Account metadata
is alongside it. Existing rclone remotes are discovered without modification;
explicit adoption adds Nixe mount preferences, without copying credentials or
changing their original configuration. External reauthentication/removal stays
with the original configuration tool. Managed accounts can be reauthenticated
in Settings: the new connection is staged and tested before replacing credentials.

Stable mounts are `$XDG_DATA_HOME/nixe/cloud/mounts/<UUID>`. Their stable paths
work with bookmarks and ordinary desktop apps; runtime socket paths are used
only for IPC. Flatpak apps may need a file portal grant to access mounted files.
Each account has an isolated `$XDG_CACHE_HOME/nixe/cloud/<UUID>` VFS cache.
The 2 GiB default is an eviction target, not a hard quota. Open or dirty files
can exceed it. Full VFS caching allows sparse, partial reads where the renderer
and provider support them.

A completed filesystem copy may still await upload. Disconnect checks rclone's
upload queue and refuses pending/failed uploads. Never clear dirty caches by hand.
Service shutdown preserves cached data for rclone recovery on the next connection.
An unexpected mount-process exit gets bounded reconnect attempts; authentication
errors wait for user action. A mounted account is probed periodically in the
background; rclone handles transient network reconnection within a live mount.

## Cloud1 public interface

Bus/interface: `io.github.design_nexus.nixe.Cloud1`.
Object: `/io/github/design_nexus/nixe/Cloud1`.

| Method | Input | Result |
| --- | --- | --- |
| ListRemotes | — | JSON array of public `Remote` metadata |
| Discover | — | JSON array of external names/provider types |
| Adopt | external remote name | stable UUID |
| Mount / Unmount / TestConnection | UUID | operation UUID |
| Cancel | operation UUID | — |
| UpdatePreferences | UUID, display name, preferences JSON | — |
| Remove | UUID, delete-local-configuration boolean | — |
| BeginSetup | — | private setup socket path |

Read-only property: `RcloneVersion` reports the detected rclone version.

Signals: `Changed()` and `OperationFinished(operation, account, success, message)`.
Rust types in `src/lib.rs` define metadata/preferences. Consumers must tolerate
unknown connection states and added JSON fields. Breaking changes require a new
D-Bus interface version. Calls never return configuration dumps, passwords, OAuth
tokens, or SSH key contents. `Remove` never invokes delete/purge against cloud data.

Setup uses one owner-only runtime Unix socket per session. The same-user peer
exchanges newline-delimited JSON (`providers`, `configure`, `oauth`, `finish`).
Configure drives rclone's returned State/Option questions; the client returns the
answer using opt.continue/state/result. The graphical client uses inherited
stream state, not subprocess arguments or D-Bus strings, for credentials.
Reauthentication supplies an existing managed UUID as `replace` on the private
configure request. No general-purpose RC or shell-command proxy is exposed.

## Security and limitations

rclone obscured passwords are reversible; file permissions are not encryption.
OAuth tokens remain in rclone's configuration. Other processes running as the same
user can access the user's files and private RC sockets. RC sockets never bind a
network port; provider OAuth may use rclone's loopback callback server.

This release does not automatically enable encrypted configurations or store a
master password in Secret Service. Encrypted external configurations may need
unlocking through rclone's existing mechanisms before adoption/mounting. No GOA
credential reuse is assumed. Credential-bearing rclone errors and RC response
bodies are suppressed from logs and ordinary IPC.

Quota display, sharing links, offline pinning, cache clearing, and cloud-wide
search are not implemented. Providers retain their own move/resume semantics.
Live third-party OAuth requires user authorization and is not covered by the
credential-free integration test.

## Targeted integration check

```
cargo build --locked
dbus-run-session -- python3 tests/integration.py
```

The check uses a temporary HOME/config/cache/runtime and disposable alias remotes.
It verifies discovery, adoption, fresh setup, permissions, multiple accounts,
reauthentication, login mounting, FUSE reads/writes/rename/delete, safe disconnect,
configuration-only removal, cancelled setup cleanup, local WebDAV onboarding,
and expired authentication. No real accounts are read or changed.
