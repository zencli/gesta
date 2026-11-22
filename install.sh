#!/bin/sh
# Install gesta with one command.
#
#   curl -fsSL https://raw.githubusercontent.com/zencli/gesta/main/install.sh | sh
#
# Environment:
#   GESTA_VERSION   version to install, or `main` (default) for the current branch
#   GESTA_PREFIX    where the binary goes (default: ~/.local, so the binary is ~/.local/bin/gesta)
#   GESTA_FROM      `source` to build with cargo even where a prebuilt binary exists
#
# Why a shell script and not a package: the binary is one file with no runtime, so the whole
# install is a download and a chmod. A package manager would add a dependency and a delay for
# no gain on Linux and macOS.
set -eu

REPO="zencli/gesta"
# `main` means "the current branch", which has no release assets and downloads nothing. A release
# install needs a tag, so an explicit version wins, and the fallback asks GitHub for the newest tag
# instead of guessing a name that will 404.
VERSION="${GESTA_VERSION:-}"
PREFIX="${GESTA_PREFIX:-$HOME/.local}"
FROM="${GESTA_FROM:-auto}"
BINDIR="$PREFIX/bin"
TARGET="$BINDIR/gesta"

say() { printf '%s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required and was not found on PATH"
}

# The asset name a release carries for this machine. Kept in one place, and the shape is
# `gesta-<version>-<platform>.tar.gz` (`.zip` for Windows), which is what the release workflow writes.
# This mapping and that workflow are the two ends of one name: when they disagree every download is a
# 404, and the 404 reads as "no release for your machine" rather than as a filename mismatch.
platform() {
    os=$(uname -s 2>/dev/null || echo unknown)
    arch=$(uname -m 2>/dev/null || echo unknown)
    case "$os" in
        Linux) os_part=unknown-linux-gnu; plat_ext=.tar.gz ;;
        Darwin) os_part=apple-darwin; plat_ext=.tar.gz ;;
        MINGW*|MSYS*|CYGWIN*) os_part=pc-windows-msvc; plat_ext=.zip ;;
        *) die "unsupported system: $os. Build from source instead: cargo install --git https://github.com/$REPO.git" ;;
    esac
    case "$arch" in
        x86_64|amd64) arch_part=x86_64 ;;
        aarch64|arm64) arch_part=aarch64 ;;
        *) die "unsupported architecture: $arch. Build from source instead: cargo install --git https://github.com/$REPO.git" ;;
    esac
    printf '%s-%s' "$arch_part" "$os_part"
}

# The archive extension for this machine, asked separately because `platform` runs in a command
# substitution and anything it sets in its own subshell is gone by the time the caller looks.
archive_ext() {
    case "$(uname -s 2>/dev/null || echo unknown)" in
        MINGW*|MSYS*|CYGWIN*) printf '.zip' ;;
        *) printf '.tar.gz' ;;
    esac
}

# The newest release tag, asked of the API rather than guessed. No tag means nothing to prebuild.
latest_tag() {
    curl -fsSL --retry 2 --max-time 30 \
        "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1
}

install_binary() {
    src="$1"
    mkdir -p "$BINDIR"
    tmp="$BINDIR/.gesta.$$"
    cp "$src" "$tmp" || die "cannot write to $BINDIR. Set GESTA_PREFIX to a directory you own."
    chmod 755 "$tmp"
    mv "$tmp" "$TARGET"
}

# A prebuilt binary needs no toolchain, so try it before asking for Rust. A machine without
# cargo can still install, which is the point of one command.
#
# The asset is an archive, not a bare binary: `gesta-<version>-<platform>.tar.gz`. Unpacking is what
# the release workflow's shape implies, and skipping it was why this path returned 404 for every
# machine and then fell through to building from source.
try_prebuilt() {
    [ "$FROM" = "source" ] && return 1
    [ -z "$VERSION" ] && VERSION=$(latest_tag)
    if [ -z "$VERSION" ]; then
        say "No release found; building from source"
        return 1
    fi
    plat=$(platform)
    ext=$(archive_ext)
    # The tag carries a `v` and the asset name does not: `v0.11.1` is the tag, `gesta-0.11.1-...` is
    # the file. Stripping it here is what makes the URL resolve, and getting it wrong produced a 404
    # that read as "no release for your machine".
    bare=${VERSION#v}
    name="gesta-$bare-$plat$ext"
    url="https://github.com/$REPO/releases/download/$VERSION/$name"
    tmpdir=$(mktemp -d)
    say "Trying $name"
    if curl -fsSL --retry 2 --max-time 300 -o "$tmpdir/$name" "$url" 2>/dev/null; then
        case "$ext" in
            .zip)
                command -v unzip >/dev/null 2>&1 || { rm -rf "$tmpdir"; say "unzip is missing; building from source"; return 1; }
                unzip -q "$tmpdir/$name" -d "$tmpdir/x" 2>/dev/null || { rm -rf "$tmpdir"; return 1; }
                ;;
            *)
                tar -xzf "$tmpdir/$name" -C "$tmpdir" 2>/dev/null || { rm -rf "$tmpdir"; return 1; }
                mkdir -p "$tmpdir/x" && mv "$tmpdir"/gesta "$tmpdir/x/" 2>/dev/null
                ;;
        esac
        bin=$(find "$tmpdir" -type f \( -name gesta -o -name gesta.exe \) -perm -u+x 2>/dev/null | head -1)
        [ -z "$bin" ] && bin=$(find "$tmpdir" -type f \( -name gesta -o -name gesta.exe \) 2>/dev/null | head -1)
        if [ -n "$bin" ]; then
            install_binary "$bin"
            rm -rf "$tmpdir"
            say "Installed $VERSION from the prebuilt archive"
            return 0
        fi
        rm -rf "$tmpdir"
        say "The archive did not contain the binary; building from source"
        return 1
    fi
    rm -rf "$tmpdir"
    say "No prebuilt archive at $VERSION for $plat; building from source"
    return 1
}

build_from_source() {
    if ! command -v cargo >/dev/null 2>&1; then
        die "no prebuilt binary for this machine and no Rust to build one.
Install Rust (one command, no administrator): curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
Then run this installer again."
    fi
    # `--branch main` fails against a git remote that has no such remote-tracking ref, so the
    # default branch is requested by leaving the flag out and a version by `--tag`. An empty VERSION
    # means "newest release", which is what a source build should follow when no tag was given.
    if [ -z "$VERSION" ]; then
        VERSION=$(latest_tag)
    fi
    if [ -z "$VERSION" ] || [ "$VERSION" = "main" ]; then
        ref=""
    else
        ref="--tag $VERSION"
    fi
    # --locked first, so a build gets the dependency versions the project was tested with.
    # shellcheck disable=SC2086
    cargo install --git "https://github.com/$REPO.git" $ref --root "$PREFIX" --locked 2>/dev/null \
        || cargo install --git "https://github.com/$REPO.git" $ref --root "$PREFIX" \
        || die "cargo install failed. Run 'rustup update' and try again."
}

main() {
    if try_prebuilt; then :; else build_from_source; fi
    [ -x "$TARGET" ] || TARGET="$TARGET.exe"
    [ -x "$TARGET" ] || die "the binary did not land at $BINDIR"

    say ""
    say "gesta installed: $TARGET"
    case ":$PATH:" in
        *":$BINDIR:"*) say "Run: gesta --help" ;;
        *)
            say ""
            say "$BINDIR is not on PATH. Add it:"
            say "  echo 'export PATH=\"$BINDIR:\$PATH\"' >> ~/.profile && . ~/.profile"
            ;;
    esac
    say ""
    say "Then in a project:"
    say "  gesta init"
    say "  gesta changeset create \"first change\" --author me"
    say "  gesta status"
}

main
