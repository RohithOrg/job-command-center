#!/bin/bash
# Build Job Command Center.app and a .dmg on macOS (no extra tools: cargo, sips-free, hdiutil).
# CI uses cargo-packager for all platforms; this is the quick local equivalent.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
cargo build --release
APP="dist/Job Command Center.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/job-command-center "$APP/Contents/MacOS/"
cp assets/icon.icns "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Job Command Center</string>
  <key>CFBundleDisplayName</key><string>Job Command Center</string>
  <key>CFBundleIdentifier</key><string>com.jobcommandcenter.desktop</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleExecutable</key><string>job-command-center</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
# Ad-hoc signature so Gatekeeper accepts a locally built copy; distribution needs a Developer ID.
codesign --force --deep --sign - "$APP"
DMG="dist/Job-Command-Center-${VERSION}-macos.dmg"
rm -f "$DMG"
hdiutil create -size 400m -volname "Job Command Center" -srcfolder "$APP" -ov -format UDZO "$DMG" >/dev/null
echo "Built $APP and $DMG"
