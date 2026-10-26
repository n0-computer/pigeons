#!/usr/bin/env bash
# Build the Rust core for iOS devices and the simulator and bundle it as
# mobile/ios/PigeonsCore.xcframework (static library + header + module map).
#
#   mobile/scripts/build-core.sh            # release
#   PROFILE=debug mobile/scripts/build-core.sh
#
# Needs Xcode (for the iOS SDKs) and the Rust targets, which are added when missing.
set -euo pipefail

mobile="$(cd "$(dirname "$0")/.." && pwd)"
core="$mobile/core"
out="$mobile/ios/PigeonsCore.xcframework"
profile="${PROFILE:-release}"

export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-17.0}"

cargo_flags=()
[[ "$profile" == release ]] && cargo_flags+=(--release)

# aarch64-apple-ios-sim covers Apple silicon Macs; add x86_64-apple-ios and
# lipo the two simulator slices together if you build on an Intel Mac.
targets=(aarch64-apple-ios aarch64-apple-ios-sim)
for target in "${targets[@]}"; do
  rustup target list --installed | grep -qx "$target" || rustup target add "$target"
  echo "==> cargo build --target $target ($profile)"
  (cd "$core" && cargo build "${cargo_flags[@]}" --target "$target")
done

# Ask cargo where it put the artifacts: CARGO_TARGET_DIR or a `build.target-dir`
# in a cargo config moves them out of $core/target.
target_dir="$(cd "$core" && cargo metadata --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[[ -n "$target_dir" ]] || { echo "could not read target_directory from cargo metadata" >&2; exit 1; }

lib=libpigeons_mobile.a
libs=()
for target in "${targets[@]}"; do
  path="$target_dir/$target/$profile/$lib"
  if [[ ! -f "$path" ]]; then
    echo "missing $path" >&2
    echo "cargo built into $target_dir; check CARGO_TARGET_DIR / build.target-dir" >&2
    exit 1
  fi
  echo "    $(lipo -info "$path")"
  libs+=(-library "$path" -headers "$core/include")
done

rm -rf "$out"
echo "==> xcodebuild -create-xcframework"
xcodebuild -create-xcframework "${libs[@]}" -output "$out"
echo "built $out"
