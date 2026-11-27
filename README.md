# SecSetter

**Finish setting up a machine after you sync your dotfiles.**

SecSetter gives you one terminal screen for the setup steps that still need you: install a config, paste an API token, restore a private file, or sign in to a service. The list shows what is in place and what still needs attention.

![SecSetter terminal UI with Git Creds and VPN sections above the default sections. GitHub sign-in is selected and marked Login needed.](docs/secsetter.png)

## When to use it

- **Set up a new laptop.** Install your usual configs, add a package registry token, and run your GitHub sign-in from one list.
- **Restore a private config.** Fetch a backed-up file from [fnox](https://fnox.jdx.dev/) and write it where the application expects it.
- **Repeat the same setup across machines.** Keep the SecSetter TOML config and non-secret templates in your dotfiles. Put real secrets in the application files or your secret store.

SecSetter can write a whole secret file or update one string field in TOML, YAML, or JSON. Login actions run commands in order. Optional readiness checks can inspect local files, command exit codes, or JSON output.

## Install

### mise (recommended)

Install the latest release globally with [mise](https://mise.jdx.dev/):

```sh
mise use -g github:prepin/secsetter@latest
```

### Manual download

Download an archive and `SHA256SUMS` from [GitHub Releases](https://github.com/prepin/secsetter/releases).

| System | Archive |
| --- | --- |
| Linux x86-64 (AMD64) | `secsetter-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `secsetter-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `secsetter-aarch64-apple-darwin.tar.gz` |

Check the archive against `SHA256SUMS` with `sha256sum <archive>` on Linux or `shasum -a 256 <archive>` on macOS. Extract the archive. Put `secsetter` in a directory on your `PATH`.

## Quick start

1. Save this example as `secsetter.toml`:

   ```toml
   [[entries]]
   id = "example-token"
   label = "Example API token"
   kind = "secret-file"
   section = "Example"
   destination = "$XDG_CONFIG_HOME/secsetter-demo/token"
   url = "https://example.com/settings/tokens"
   ```

2. Validate the config and open the terminal UI:

   ```sh
   secsetter -c ./secsetter.toml validate
   secsetter -c ./secsetter.toml
   ```

3. Use ↑/↓ or `j`/`k` to select **Example API token**. Press Enter to open the masked prompt. Paste a dummy token. Press Enter to save it. To clear it, press Delete on the entry and confirm.

The `url` in this example is a placeholder. If an entry has a token page, press `o` in the list to open it in your browser. In the secret popup, click **Open token page** or press **Ctrl+O**. The popup keeps any text you entered.

## Config reference

The default config is `$XDG_CONFIG_HOME/secsetter/config.toml`. If `XDG_CONFIG_HOME` is unset or empty, the default is `~/.config/secsetter/config.toml`. To select a different config, put `-c <file>` before the command. Relative config paths start at the current directory.

Each `[[entries]]` table needs a unique, non-empty `id`, a `label`, and a `kind`.

| `kind` | Required fields | Action |
| --- | --- | --- |
| `template` | `source`, `destination` | Install a non-secret file. The `source` path is relative to `templates/` beside the selected config. |
| `secret-field` | `destination`, `format`, `key` | Set a string field in an existing TOML, YAML, or JSON file. |
| `secret-file` | `destination` | Write a whole file from secret input. |
| `fnox-file` | `source`, `destination` | Restore a whole file from fnox. The `source` is a literal fnox key. The restore requires `fnox` on `PATH`. |
| `login` | `commands` | Run login steps in the current terminal. Add `ready_when` to show a login status. |

| Optional field | Applies to | Meaning |
| --- | --- | --- |
| `section` | All entries | Group related entries of any kind in the terminal UI. Use a non-blank name without control characters. |
| `platforms` | All entries | Use `["linux"]`, `["macos"]`, or both. Omit it to include the entry on both platforms. |
| `url` | `secret-file`, `secret-field` | Link to a token page. Use an absolute HTTP or HTTPS URL without embedded credentials. Keep token values out of the URL. |
| `privileged`, `owner`, `group` | File entries | If a file operation needs elevation, set `privileged = true`. Supply the Unix user and group names in `owner` and `group`. |
| `ready_when` | `login` | Run the optional readiness checks described below. |

Use an absolute path or start with `~/` or `$XDG_CONFIG_HOME/` for a `destination` or a file check's `path`. The `-c` option does not change how these prefixes expand. For a `secret-field` or `field-set` check, use `format = "toml"`, `"yaml"`, or `"json"`. Write `key` as an array such as `["credentials", "token"]`. A dot inside a key is literal. Key paths cannot traverse arrays. For a `secret-field` write, the file and all parent objects must exist.

### Sections

Use the same `section = "Git Creds"` on a template, a token, and a login to show them together. Names are case-sensitive. Custom sections appear first and sort alphabetically without regard to case. The default sections follow in order: **Templates**, **Secrets** (fields), **Files** (whole-file secrets), **Logins**, and **Fnox**.

The terminal UI hides empty sections and keeps config order within each section. Status icons share one column. `secsetter list` keeps the original config order and has no section column.

### Login readiness

Put each login step in its own argument array in `commands`, even for a single step. Steps run in order and stop on the first failure or interruption. SecSetter passes arguments literally. If a step needs pipes or other shell syntax, specify a shell such as `sh -c` in the array.

| `ready_when` check | Fields and result |
| --- | --- |
| `file-exists` | Set `path` to a regular file. An empty file passes. |
| `field-set` | Set `path`, `format`, and `key`. A non-empty string at that key passes. |
| `command` | Set `command` to one argument array. By default, exit `0` passes and a nonzero exit means **Login needed**. Add `json` to match structured output. |

If all checks pass, SecSetter shows **Login configured**. If any check fails, it shows **Login needed**. If the status is unknown or `ready_when` is absent or empty, the terminal UI shows `¯\_(ツ)_/¯`. In `secsetter list`, unknown statuses still read **Unknown**, and login entries without checks omit the status column. You can run a login action at any status.

Use noninteractive status commands from configs you trust. `secsetter list` runs the checks on each invocation. The terminal UI runs command checks in the background at startup and refreshes the entry after a login attempt.

SecSetter disconnects standard input and discards standard error. It gives each command five seconds. If a command cannot start, ends by signal, or times out, the check reports **Unknown**.

### Match JSON output

Keep exit-code checks for tools such as `codex login status`. If exit codes alone are insufficient, request JSON in the command and set `json.matches`. For example, this entry matches a saved OpenAI OAuth connection in OpenCode:

```toml
[[entries]]
id = "opencode-login"
label = "OpenCode OpenAI OAuth"
kind = "login"
commands = [["opencode", "auth", "login", "openai"]]
ready_when = [
  { kind = "command", command = ["opencode", "auth", "list", "--format", "json"], json = { matches = [{ id = "openai", connections = [{ type = "credential", method = "oauth" }] }], on_miss = "login-needed" } },
]
```

An object pattern checks the named fields and ignores extra fields. An array pattern contains one pattern and requires one matching element. All fields in an object pattern must match within the same element. Strings, booleans, integers, and finite floats match by type and value.

SecSetter discards standard output for exit-code checks. For JSON checks it reads up to 64 KiB into memory and never displays or logs the captured output.

Exit `0` and a match pass the check. A compatible response with no match shows **Unknown** by default (`on_miss = "unknown"`). Set `on_miss = "login-needed"` inside `json` to report **Login needed** for that case. A nonzero exit, oversized output, invalid JSON, or an unexpected response shape shows **Unknown**.

The OpenCode example checks for a saved connection. It does not test whether the token still works. The [GitLab example](skills/secsetter/SKILL.md#example-config) uses `glab auth status --hostname <host> --output json` and matches `state = "success"` for that host. It reports **Unknown** when the response cannot distinguish a rejected token from an unreachable host.

## Command line

Use entry IDs in scripts. `validate` reads only the config and checks entries for all platforms. It does not inspect destinations or run any actions or readiness commands.

```sh
secsetter -c ./secsetter.toml list
secsetter -c ./secsetter.toml set example-token < ./token.txt
secsetter --help
secsetter login --help
secsetter skill
```

Use `install <id>` for templates, `restore <id>` for fnox files, and `login <id>` for login actions. If the destination already exists, `install` and `restore` require `--replace`. `clear <id>` empties a secret field or removes the local secret file without a confirmation prompt.

`secsetter skill` prints the [config-writing guide](skills/secsetter/SKILL.md) with copyable examples for every entry kind. Help and the guide work without a config.

## Contributing

Use `cargo test --locked` to run the test suite. For bug reports and feature requests, open a [GitHub issue](https://github.com/prepin/secsetter/issues).

## Release

Update the version in `Cargo.toml` and refresh `Cargo.lock`. Merge the changes into `master`. When CI passes, push a matching version tag such as `v1.1.0`. GitHub Actions builds and tests all three targets before it publishes the archives and checksums.

## License

SecSetter is available under the [MIT License](LICENSE).
