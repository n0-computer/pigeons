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

lib=libpigeons_mobile.a
rm -rf "$out"
echo "==> xcodebuild -create-xcframework"
xcodebuild -create-xcframework \
  -library "$core/target/aarch64-apple-ios/$profile/$lib" -headers "$core/include" \
  -library "$core/target/aarch64-apple-ios-sim/$profile/$lib" -headers "$core/include" \
  -output "$out"
echo "built $out"
