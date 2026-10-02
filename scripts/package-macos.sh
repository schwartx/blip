#!/bin/bash
# Build a native macOS bundle. Developer ID signing and notarization belong to
# the distribution pipeline; this script does not claim either.
set -euo pipefail

usage() {
    printf 'Usage: %s [--target <Rust target>] [--output <directory>] [--no-build]\n' "$0"
}

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
target=''
output="$repo_root/dist"
build=true
while [[ $# -gt 0 ]]; do
    case "$1" in
        --target|--output)
            if [[ $# -lt 2 || -z "$2" ]]; then
                usage >&2
                exit 2
            fi
            if [[ "$1" == --target ]]; then target=$2; else output=$2; fi
            shift 2
            ;;
        --no-build) build=false; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

if [[ $(uname -s) != Darwin ]]; then
    printf 'Packaging requires macOS (the macOS SDK and ditto).\n' >&2
    exit 1
fi

case "$target" in
    ''|aarch64-apple-darwin|x86_64-apple-darwin) ;;
    *) printf 'Unsupported macOS target: %s\n' "$target" >&2; exit 2 ;;
esac

cd "$repo_root"
# Keep the binary deployment target aligned with LSMinimumSystemVersion.
export MACOSX_DEPLOYMENT_TARGET=11.0
if [[ "$build" == true ]]; then
    if [[ -n "$target" ]]; then
        cargo build --release --locked --target "$target"
    else
        cargo build --release --locked
    fi
fi

target_dir=${CARGO_TARGET_DIR:-"$repo_root/target"}
if [[ "$target_dir" != /* ]]; then target_dir="$repo_root/$target_dir"; fi
bin_dir="$target_dir${target:+/$target}/release"
for executable in blip blipd; do
    if [[ ! -x "$bin_dir/$executable" ]]; then
        printf 'Missing executable: %s\n' "$bin_dir/$executable" >&2
        exit 1
    fi
    if [[ $(file -b "$bin_dir/$executable") != *Mach-O* ]]; then
        printf 'Expected a macOS Mach-O executable: %s\n' "$bin_dir/$executable" >&2
        exit 1
    fi
done

case "${target:-$(uname -m)}" in
    aarch64-apple-darwin|arm64) arch=arm64 ;;
    x86_64-apple-darwin|x86_64) arch=x86_64 ;;
    *) printf 'Unsupported architecture. Pass --target explicitly.\n' >&2; exit 2 ;;
esac
version=$(awk '/^version[[:space:]]*=/ {split($0, fields, "\""); print fields[2]; exit}' Cargo.toml)
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    printf 'Bundle versions require a numeric major.minor.patch version: %s\n' "$version" >&2
    exit 1
fi

mkdir -p "$output"
output=$(cd "$output" && pwd)
stage=$(mktemp -d "$output/.blip-macos.XXXXXX")
trap 'rm -rf "$stage"' EXIT
app="$stage/Blip.app"
mkdir -p "$app/Contents/MacOS"
cp "$bin_dir/blip" "$bin_dir/blipd" "$app/Contents/MacOS/"
sed "s/@VERSION@/$version/g" macos/Info.plist > "$app/Contents/Info.plist"
/usr/bin/plutil -lint "$app/Contents/Info.plist"

archive="$output/blip-$version-macos-$arch.zip"
# Stage the archive before replacing a previous generated archive.
/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$app" "$stage/archive.zip"
mv "$stage/archive.zip" "$archive"
printf 'Created %s\n' "$archive"
printf 'Local development bundle; no Developer ID signature or notarization.\n'
