# SecSetter

SecSetter installs config templates, writes secrets to the files that use them, restores files from fnox, and runs interactive login actions. It has a terminal UI and a command-line interface.

## Install

Download an archive and `SHA256SUMS` from [GitHub Releases](https://github.com/prepin/secsetter/releases).

| System | Archive |
| --- | --- |
| Linux x86-64 (AMD64) | `secsetter-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `secsetter-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `secsetter-aarch64-apple-darwin.tar.gz` |

Compare the archive's SHA-256 checksum with its line in `SHA256SUMS`. Use `sha256sum <archive>` on Linux or `shasum -a 256 <archive>` on macOS. Extract the archive and put `secsetter` in a directory on your `PATH`.

## Use

```sh
secsetter --help
secsetter skill
secsetter -c ./draft.toml validate
secsetter
```

`secsetter skill` prints the [config-writing guide](skills/secsetter/SKILL.md). By default, SecSetter reads `$XDG_CONFIG_HOME/secsetter/config.toml` or `~/.config/secsetter/config.toml`. Commands that restore files from fnox require `fnox` on `PATH`.

## Release

Update the version in `Cargo.toml` and refresh `Cargo.lock`. Merge those changes into `main` and check that CI passes. Push a tag that matches the Cargo version, such as `v1.0.0`. GitHub Actions builds and tests all three targets, then publishes the archives and checksums.
