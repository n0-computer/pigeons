#!/usr/bin/env bash
# Archive the app and hand it to App Store Connect for TestFlight.
#
#   PIGEONS_TEAM_ID=ABCDE12345 mobile/scripts/archive.sh            # archive + upload
#   PIGEONS_TEAM_ID=ABCDE12345 mobile/scripts/archive.sh --no-upload  # archive + export .ipa only
#
# Uploading uses the Apple ID signed in to Xcode. For CI, pass an App Store
# Connect API key instead by exporting ASC_KEY_ID, ASC_ISSUER_ID and
# ASC_KEY_PATH (the .p8 file).
#
# Runs build-core.sh and xcodegen first so the archive always matches the
# sources. Bump CURRENT_PROJECT_VERSION in ios/project.yml between uploads;
# App Store Connect rejects a build number it has seen.
set -euo pipefail

: "${PIGEONS_TEAM_ID:?set PIGEONS_TEAM_ID to your Apple Developer team id}"
mobile="$(cd "$(dirname "$0")/.." && pwd)"
ios="$mobile/ios"
build="$ios/build"
upload=1
[[ "${1:-}" == "--no-upload" ]] && upload=0

command -v xcodegen >/dev/null || { echo "xcodegen not found: brew install xcodegen" >&2; exit 1; }

"$mobile/scripts/build-core.sh"
(cd "$ios" && xcodegen generate)

mkdir -p "$build"
auth=()
if [[ -n "${ASC_KEY_ID:-}" ]]; then
  auth=(-authenticationKeyID "$ASC_KEY_ID" -authenticationKeyIssuerID "$ASC_ISSUER_ID" -authenticationKeyPath "$ASC_KEY_PATH")
fi

echo "==> archive"
xcodebuild -project "$ios/Pigeons.xcodeproj" -scheme Pigeons -configuration Release \
  -destination 'generic/platform=iOS' -archivePath "$build/Pigeons.xcarchive" \
  -allowProvisioningUpdates "${auth[@]}" archive

destination=export
[[ $upload -eq 1 ]] && destination=upload
cat > "$build/ExportOptions.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key><string>app-store-connect</string>
  <key>destination</key><string>$destination</string>
  <key>teamID</key><string>$PIGEONS_TEAM_ID</string>
  <key>signingStyle</key><string>automatic</string>
  <key>uploadSymbols</key><true/>
  <key>manageAppVersionAndBuildNumber</key><true/>
</dict>
</plist>
PLIST

echo "==> export ($destination)"
xcodebuild -exportArchive -archivePath "$build/Pigeons.xcarchive" \
  -exportOptionsPlist "$build/ExportOptions.plist" -exportPath "$build/export" \
  -allowProvisioningUpdates "${auth[@]}"

if [[ $upload -eq 1 ]]; then
  echo "uploaded; it appears in App Store Connect > TestFlight once processing finishes"
else
  echo "exported to $build/export"
fi
