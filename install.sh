#!/bin/sh
# Install or update viper on Linux or macOS:
#
#   curl -fsSL https://raw.githubusercontent.com/stephenberry/viper/main/install.sh | sh
#
# Environment:
#   VIPER_VERSION         release to install, e.g. v0.1.1 (default: the latest)
#   VIPER_INSTALL_DIR     where to put the binary (default: ~/.local/bin)
#   VIPER_NO_MODIFY_PATH  if set, print the line that adds the binary to PATH instead of
#                         appending it to your shell's startup file

set -eu

REPO="stephenberry/viper"

say() {
    printf '%s\n' "$*"
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

# HTTPS only, failing on HTTP errors.
fetch() {
    curl --proto '=https' --tlsv1.2 -fsSL "$@"
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux)
            case "$arch" in
                x86_64 | amd64) echo x86_64-unknown-linux-musl ;;
                aarch64 | arm64) echo aarch64-unknown-linux-musl ;;
                *) die "no prebuilt viper for Linux on $arch; build from source: https://github.com/$REPO#install" ;;
            esac
            ;;
        Darwin) echo universal-apple-darwin ;;
        MINGW* | MSYS* | CYGWIN*) die "on Windows, run in PowerShell: irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" ;;
        *) die "no prebuilt viper for $os; build from source: https://github.com/$REPO#install" ;;
    esac
}

# The tag of the latest release, read from where /releases/latest redirects.
latest_version() {
    url=$(fetch -I -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
        die "could not reach GitHub to find the latest release"
    version=${url##*/}
    case "$version" in
        v*) echo "$version" ;;
        *) die "no viper release found at https://github.com/$REPO/releases" ;;
    esac
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        die "need sha256sum or shasum to verify the download"
    fi
}

main() {
    command -v curl >/dev/null 2>&1 || die "curl is required"
    command -v tar >/dev/null 2>&1 || die "tar is required"

    target=$(detect_target)
    version=${VIPER_VERSION:-$(latest_version)}
    case "$version" in
        v*) ;;
        *) version="v$version" ;;
    esac
    install_dir=${VIPER_INSTALL_DIR:-$HOME/.local/bin}
    name="viper-$version-$target"
    base="https://github.com/$REPO/releases/download/$version"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' INT TERM

    say "Downloading viper $version for $target"
    fetch -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" || die "could not download $base/$name.tar.gz"
    fetch -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "could not download $base/SHA256SUMS"

    expected=$(awk -v file="$name.tar.gz" '$2 == file { print $1 }' "$tmp/SHA256SUMS")
    [ -n "$expected" ] || die "SHA256SUMS has no entry for $name.tar.gz"
    [ "$(sha256 "$tmp/$name.tar.gz")" = "$expected" ] || die "checksum mismatch for $name.tar.gz"

    tar xzf "$tmp/$name.tar.gz" -C "$tmp"
    mkdir -p "$install_dir"
    # Copy beside the destination, then rename over it, so a running viper is replaced cleanly.
    cp "$tmp/$name/viper" "$install_dir/.viper.new"
    chmod 755 "$install_dir/.viper.new"
    mv -f "$install_dir/.viper.new" "$install_dir/viper"

    say "Installed $("$install_dir/viper" --version) to $install_dir/viper"

    case ":$PATH:" in
        *":$install_dir:"*) ;;
        *) add_to_path "$install_dir" ;;
    esac
}

# The startup file of the user's login shell, or nothing if the shell is unknown.
shell_rc() {
    case "${SHELL:-}" in
        */zsh) echo "${ZDOTDIR:-$HOME}/.zshrc" ;;
        # macOS terminals start bash as a login shell, which reads .bash_profile, not .bashrc.
        */bash) if [ "$(uname -s)" = Darwin ]; then echo "$HOME/.bash_profile"; else echo "$HOME/.bashrc"; fi ;;
        */fish) echo "${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish" ;;
    esac
}

# Adds a directory to PATH in the user's shell startup file, unless VIPER_NO_MODIFY_PATH is set.
# Falls back to printing the line to add when the file can't be determined or written.
add_to_path() {
    dir=$1
    rc=$(shell_rc)
    case "$rc" in
        */config.fish) line="fish_add_path \"$dir\"" ;;
        *) line="export PATH=\"$dir:\$PATH\"" ;;
    esac

    # Characters that would break out of the double-quoted line are left for the user to handle.
    # shellcheck disable=SC2016 # the $ and ` are literal characters to match
    if [ -n "${VIPER_NO_MODIFY_PATH:-}" ] || [ -z "$rc" ] || printf '%s' "$dir" | grep -q '["$`\\]'; then
        target=${rc:-"your shell's startup file"}
        say ""
        say "$dir is not on your PATH. Add this line to $target, then open a new shell:"
        say ""
        say "  $line"
        return
    fi

    if [ -f "$rc" ] && grep -qxF "$line" "$rc"; then
        say ""
        say "$rc already adds $dir to your PATH; open a new shell to use viper."
        return
    fi

    if ! mkdir -p "$(dirname "$rc")" 2>/dev/null || ! {
        # Separate from existing content by a blank line, even if the file lacks a final newline.
        if [ -s "$rc" ]; then
            [ -z "$(tail -c 1 "$rc")" ] || printf '\n'
            printf '\n'
        fi
        printf '# Added by the viper installer\n%s\n' "$line"
    } 2>/dev/null >>"$rc"; then
        say ""
        say "Could not update $rc. Add this line to it, then open a new shell:"
        say ""
        say "  $line"
        return
    fi

    say ""
    say "Added $dir to your PATH in $rc. Open a new shell, or run this to use viper now:"
    say ""
    say "  $line"
}

main "$@"
