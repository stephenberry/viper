# Installing viper

## Quick install

On Linux or macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/stephenberry/viper/main/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/stephenberry/viper/main/install.ps1 | iex
```

The installer downloads the latest release for your platform, checks it against the release's `SHA256SUMS`, and installs it:

| Platform | Location |
|---|---|
| Linux, macOS | `~/.local/bin/viper`. If that directory is not on your `PATH`, the installer prints the line to add. |
| Windows | `%LOCALAPPDATA%\Programs\viper\viper.exe`, which is added to your user `PATH` |

Run the same command again to update; your configuration in `~/.viper` is kept. To change what is installed, set these first:

| Variable | Effect |
|---|---|
| `VIPER_VERSION` | Release to install, such as `v0.1.1` (default: the latest) |
| `VIPER_INSTALL_DIR` | Directory for the binary |

For example, `curl -fsSL https://raw.githubusercontent.com/stephenberry/viper/main/install.sh | VIPER_VERSION=v0.1.1 sh`.

On Windows, viper runs tools through bash, so also install [Git for Windows](https://git-scm.com/download/win), which provides it. viper finds its bash automatically.

## Manual install

Each [GitHub release](https://github.com/stephenberry/viper/releases) has prebuilt binaries, with checksums in `SHA256SUMS`:

| Platform | Archive |
|---|---|
| Linux x86_64 (static, any distribution) | `viper-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 (static, any distribution) | `viper-<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS (Intel and Apple silicon) | `viper-<version>-universal-apple-darwin.tar.gz` |
| Windows x86_64 | `viper-<version>-x86_64-pc-windows-msvc.zip` |

Download the archive for your platform, check it with `sha256sum -c --ignore-missing SHA256SUMS` (`shasum -a 256 -c ...` on macOS), extract it, and put `viper` (or `viper.exe`) in a directory on your `PATH`.

If macOS blocks a binary downloaded in a browser, clear the quarantine flag: `xattr -d com.apple.quarantine /path/to/viper`.

## From source

With Rust 1.96 or newer:

```bash
cargo install --locked --git https://github.com/stephenberry/viper
```

## Setting up a model provider

viper keeps its configuration in `~/.viper` (see [Configuration](configuration.md)).

**Anthropic:** set `ANTHROPIC_API_KEY`, or run `viper` and then `/login anthropic`.

**A LiteLLM gateway:** run `viper`, then:

1. `/login litellm` and enter the gateway's base URL and your API key. The key is checked and saved to `~/.viper/auth.json`.
2. `/sync-models litellm` to add the gateway's models, with their limits and prices, to `~/.viper/models.json`.
3. `/model` to pick a model. The choice becomes the default.

**Copying an existing setup** from another machine:

```bash
mkdir -p ~/.viper && chmod 700 ~/.viper
scp you@other-machine:~/.viper/{models.json,auth.json,settings.json} ~/.viper/
chmod 600 ~/.viper/auth.json
```

`auth.json` holds your API keys, so keep it private.

If your gateway is only reachable over a VPN, make sure the VPN routes the gateway's host on this machine too; otherwise requests fail to connect.

Then `cd` into a project and run `viper`.
