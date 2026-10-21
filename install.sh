#!/bin/sh
# Copyright 2023 n0. All rights reserved. Dual MIT/Apache license.

set -e

bucket_url="https://vorc.s3.amazonaws.com"
bin_name="pigeons"

if [ "$OS" = "Windows_NT" ]; then
    echo "Error: this installer only works on linux, macOS & Android (Termux)." 1>&2
    exit 1
else
    case $(uname -sm) in
    "Darwin x86_64") target="darwin-x86_64" ;;
    "Darwin arm64") target="darwin-aarch64" ;;
    "Linux x86_64")
        if [ -f /system/build.prop ]; then
            target="android-x86_64"
        else
            target="linux-amd64"
        fi
        ;;
    "Linux arm64"|"Linux aarch64")
        if [ -f /system/build.prop ]; then
            target="android-aarch64"
        else
            target="linux-aarch64"
        fi
        ;;
    "Linux armv7l"|"Linux armv7")
        if [ -f /system/build.prop ]; then
            target="android-armv7"
        else
            echo "Unsupported platform: $(uname -sm)" 1>&2; exit 1
        fi
        ;;
    *) echo "Unsupported platform: $(uname -sm)" 1>&2; exit 1 ;;
    esac
fi

echo "Downloading $bin_name for $target"
curl -fL "${bucket_url}/${bin_name}-${target}-latest" -o "$bin_name"
chmod +x "$bin_name"

dest=/usr/local/bin
if [ ! -w "$dest" ]; then
    if command -v sudo >/dev/null 2>&1; then
        echo "Installing to $dest (requires sudo)"
        sudo mv "$bin_name" "$dest/$bin_name"
    else
        # Android (Termux): no root and no /usr/local; install into the Termux prefix.
        dest="${PREFIX:-$HOME}/bin"
        mkdir -p "$dest"
        mv "$bin_name" "$dest/$bin_name"
    fi
else
    mv "$bin_name" "$dest/$bin_name"
fi
echo "Installed to $dest/$bin_name"
