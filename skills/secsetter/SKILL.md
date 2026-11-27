---
name: secsetter
description: Write or edit a SecSetter TOML config for templates, secrets, fnox files, or login actions. Group entries, add token page links, and validate the config.
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
- For a file entry that needs elevation, set `privileged = true` and specify `owner` and `group`. Use user and group names that exist on the target platform. Login actions run as the current user.

## Sections

Set `section = "VPN"` on each entry that belongs in the **VPN** section of the terminal UI. A section can contain any mix of entry kinds. Names are case-sensitive and cannot be blank or contain control characters. The `group` field sets the Unix group for a privileged file entry.

Custom sections appear first, in alphabetical order without regard to case. Entries without `section` appear in these default sections, in order:

| Section | Entry kind |
| --- | --- |
| Templates | `template` |
| Secrets | `secret-field` |
| Files | `secret-file` |
| Logins | `login` |
| Fnox | `fnox-file` |

The terminal UI omits empty sections. Entries within each section keep their config order. `secsetter list` keeps the original order and has no section column.

## Token pages

To link to the page where you create or rotate a token, set `url` on its entry. Only `secret-field` and `secret-file` accept this optional field. Use an absolute HTTP or HTTPS URL without credentials. Keep token values out of the URL.

Press `o` on the selected entry to open the page in your default browser. In the secret popup, click **Open token page** or press **Ctrl+O**. The popup keeps any text you entered.

## Example config

Copy the entries that match the task. Replace example IDs, paths, keys, and commands with the actual values.

```toml
# Install a non-secret template or replace the destination with it.
[[entries]]
id = "example-config"
label = "Example config"
kind = "template"
section = "Example"
source = "example.toml"
destination = "$XDG_CONFIG_HOME/example/config.toml"

# Set a string field in an existing config file.
[[entries]]
id = "example-token"
label = "Token in example config"
kind = "secret-field"
section = "Example"
destination = "$XDG_CONFIG_HOME/example/config.toml"
format = "toml"
key = ["credentials", "token"]
url = "https://example.com/settings/tokens"

# Write a secret as an entire file.
[[entries]]
id = "token-file"
label = "Standalone token"
kind = "secret-file"
destination = "$XDG_CONFIG_HOME/example/token"
url = "https://example.com/settings/tokens"

# Restore an entire file from fnox.
[[entries]]
id = "vpn-profile"
label = "VPN profile"
kind = "fnox-file"
section = "VPN"
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
section = "Example"
commands = [
  ["example", "login"],
  ["example", "setup"],
]
ready_when = [
  { kind = "file-exists", path = "$XDG_CONFIG_HOME/example/session" },
  { kind = "field-set", path = "$XDG_CONFIG_HOME/example/profile.json", format = "json", key = ["account", "user"] },
]

# Run a command to check the login status.
[[entries]]
id = "codex-login"
label = "Codex sign-in"
kind = "login"
commands = [["codex", "login"]]
ready_when = [
  { kind = "command", command = ["codex", "login", "status"] },
]
```

## Fields and login checks

- For `secret-field` and `field-set`, choose `format = "toml"`, `"yaml"`, or `"json"`. List each key as a separate string. A dot within a key is literal. Key paths cannot traverse arrays. To set a field, the file and all parent objects must exist.
- A login entry requires `commands`, even for one step: `commands = [["example", "login"]]`. Each step starts with an executable. SecSetter passes the remaining arguments literally and does not interpret shell syntax. Steps run in order and stop at the first failure or interruption.
- If `ready_when` is absent or empty (`[]`), SecSetter shows no login status. If all checks pass, SecSetter shows **Login configured**. If any required file or value is absent, SecSetter shows **Login needed**. If SecSetter cannot complete a check and none fail, it shows **Unknown**.
- A `file-exists` check accepts an empty regular file. A `field-set` check requires a non-empty string.
- File checks describe local files. A command check reports the status that the command returns. A login action can run regardless of its status.

### Command checks

Use a noninteractive status command from a config you trust. Put the executable first and each argument in its own string. If the check needs shell syntax, specify a shell, such as `sh -c`, in the array.

SecSetter passes the arguments as written. It disconnects standard input and discards standard output and standard error. Exit `0` passes the check. A nonzero exit shows **Login needed**. If the command cannot start, ends by signal, or takes more than five seconds, the result is **Unknown** unless another check fails.

`secsetter list` runs the checks each time. The terminal UI caches the status from startup and refreshes it after a login attempt for that entry. `secsetter validate` validates the array without running the command.
