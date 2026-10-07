# Installing viper

Each [GitHub release](https://github.com/stephenberry/viper/releases) has prebuilt binaries:

| Platform | Archive |
|---|---|
| Linux x86_64 (static, any distribution) | `viper-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 (static, any distribution) | `viper-<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS (Intel and Apple silicon) | `viper-<version>-universal-apple-darwin.tar.gz` |
| Windows x86_64 | `viper-<version>-x86_64-pc-windows-msvc.zip` |

`SHA256SUMS` in each release lists the archives' checksums.

The commands below use the [GitHub CLI](https://cli.github.com) (`gh`), which works while the repository is private as long as you are logged in (`gh auth login`) with an account that has access. You can also download the files from the release page in a browser.

## Linux

1. Download the latest release for your CPU and verify it:

   ```bash
   case "$(uname -m)" in
     x86_64)        target=x86_64-unknown-linux-musl ;;
     aarch64|arm64) target=aarch64-unknown-linux-musl ;;
     *)             echo "no prebuilt viper for $(uname -m); build from source" ;;
   esac

   cd "$(mktemp -d)"
   gh release download -R stephenberry/viper -p "viper-*-$target.tar.gz" -p SHA256SUMS
   sha256sum -c --ignore-missing SHA256SUMS
   ```

   The check prints `OK`. To install a specific version, add its tag after `download` (for example `gh release download v0.1.0 ...`).

2. Unpack it and put `viper` on your `PATH`:

   ```bash
   tar xzf viper-*-"$target".tar.gz
   mkdir -p ~/.local/bin
   install -m 755 viper-*-"$target"/viper ~/.local/bin/viper
   viper --version
   ```

   If the shell reports `viper: command not found`, add `export PATH="$HOME/.local/bin:$PATH"` to `~/.bashrc` (or `~/.zshrc`) and open a new shell.

## macOS

```bash
cd "$(mktemp -d)"
gh release download -R stephenberry/viper -p "viper-*-universal-apple-darwin.tar.gz" -p SHA256SUMS
shasum -a 256 -c --ignore-missing SHA256SUMS
tar xzf viper-*-universal-apple-darwin.tar.gz
mkdir -p ~/.local/bin
install -m 755 viper-*-universal-apple-darwin/viper ~/.local/bin/viper
viper --version
```

If macOS blocks a binary downloaded in a browser, clear the quarantine flag: `xattr -d com.apple.quarantine ~/.local/bin/viper`.

## Windows

Download `viper-<version>-x86_64-pc-windows-msvc.zip`, extract `viper.exe` into a folder on your `PATH`, and run `viper --version`. viper runs tools through bash, so install [Git for Windows](https://git-scm.com/download/win), which provides it; viper finds its bash automatically.

## From source

With Rust 1.96 or newer:

```bash
git clone https://github.com/stephenberry/viper.git
cd viper
cargo install --path .
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

## Updating

Repeat the download and install steps; the new binary replaces the old one. Your configuration in `~/.viper` is kept.
