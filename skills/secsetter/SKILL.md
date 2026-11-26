---
name: secsetter
description: Write or edit a SecSetter TOML config for templates, secrets, fnox files, or login actions. Validate a draft with SecSetter before it goes into use.
---

# Write a SecSetter config

1. Choose the entry kind by the action the user wants. Use the examples below as a starting point.
2. Save a draft TOML file. Give each entry a unique, non-empty `id` and a human-readable `label`. Keep secret values out of the config.
3. Run `secsetter -c ./draft.toml validate`. Correct the reported errors until the command exits `0`.

Validation reads only the config and checks entries for all platforms. It does not inspect destinations, open templates, look up executables, or run actions. A valid draft can still need those files or programs at runtime.

## Paths and platforms

- The default config is `$XDG_CONFIG_HOME/secsetter/config.toml`. If `XDG_CONFIG_HOME` is unset or empty, the default is `~/.config/secsetter/config.toml`.
- Put `-c <file>` before the command to select another config. A relative config path starts at the current directory.
- Use an absolute path, `~/`, or `$XDG_CONFIG_HOME/` for destinations and readiness check paths. The `-c` option does not change how these prefixes expand.
- A template `source` is relative to `templates/` beside the selected config. A fnox `source` is the literal key that fnox uses for the stored file.
- Omit `platforms` for an entry that applies everywhere. Use `platforms = ["linux"]` or `platforms = ["macos"]` for a platform-specific entry. Use distinct IDs for separate entries.
- For a File Entry that needs elevation, set `privileged = true` and specify `owner` and `group`. Use account names that exist on the target platform. Login Actions run as the current user.

## Example config

Copy the entries that match the task. Replace example IDs, paths, keys, and commands with the actual values.

```toml
# Install a non-secret template or replace the destination with it.
[[entries]]
id = "example-config"
label = "Example config"
kind = "template"
source = "example.toml"
destination = "$XDG_CONFIG_HOME/example/config.toml"

# Set a string field in an existing config file.
[[entries]]
id = "example-token"
label = "Token in example config"
kind = "secret-field"
destination = "$XDG_CONFIG_HOME/example/config.toml"
format = "toml"
key = ["credentials", "token"]

# Write a secret as an entire file.
[[entries]]
id = "token-file"
label = "Standalone token"
kind = "secret-file"
destination = "$XDG_CONFIG_HOME/example/token"

# Restore an entire file from fnox.
[[entries]]
id = "vpn-profile"
label = "VPN profile"
kind = "fnox-file"
source = "vpn/example.conf"
destination = "/etc/wireguard/example.conf"
platforms = ["linux"]
privileged = true
owner = "root"
group = "root"

# Run the login steps in order in the user's terminal.
[[entries]]
id = "example-login"
label = "Example sign-in"
kind = "login"
commands = [
  ["example", "login"],
  ["example", "setup"],
]
ready_when = [
  { kind = "file-exists", path = "$XDG_CONFIG_HOME/example/session" },
  { kind = "field-set", path = "$XDG_CONFIG_HOME/example/profile.json", format = "json", key = ["account", "user"] },
]
```

## Fields and login checks

- For `secret-field` and `field-set`, choose `format = "toml"`, `"yaml"`, or `"json"`. List each key as a separate string. A dot within a key is literal. Key paths cannot traverse arrays. To set a field, the file and all parent objects must exist.
- A one-step Login Action still uses `commands = [["example", "login"]]`. Each step contains an executable followed by its literal arguments. SecSetter uses no implicit shell and stops at the first failure or interruption.
- Omit `ready_when` to show no login status. If all checks pass, SecSetter shows **Login configured**. If any required file or value is absent, SecSetter shows **Login needed**. If SecSetter cannot complete a check and none are known to fail, it shows **Unknown**.
- A `file-exists` check accepts an empty regular file. A `field-set` check requires a non-empty string.
- Readiness checks describe local files. They do not verify credentials. A Login Action can run regardless of its status.
