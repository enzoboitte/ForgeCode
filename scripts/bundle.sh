#!/bin/sh
# Build ForgeCode.app (release binary + icon) in target/release/bundle/.
set -eu
cd "$(dirname "$0")/.."
# TARGET=x86_64-apple-darwin builds for Intel Macs from an Apple Silicon one (the CI does).
if [ -n "${TARGET:-}" ]; then
  cargo build --release --target "$TARGET"
  bin=target/$TARGET/release/forgecode
else
  cargo build --release
  bin=target/release/forgecode
fi

out=$(dirname "$bin")/bundle/ForgeCode.app
work=$(mktemp -d)
rm -rf "$out"
mkdir -p "$out/Contents/MacOS" "$out/Contents/Resources"

# Icon: render the SVG at 1024 px, then every size macOS wants.
qlmanage -t -s 1024 -o "$work" assets/logo.svg >/dev/null
mkdir "$work/AppIcon.iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size "$work/logo.svg.png" --out "$work/AppIcon.iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double "$work/logo.svg.png" --out "$work/AppIcon.iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$work/AppIcon.iconset" -o "$out/Contents/Resources/AppIcon.icns"

cp "$bin" "$out/Contents/MacOS/ForgeCode"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$out/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>ForgeCode</string>
  <key>CFBundleDisplayName</key><string>ForgeCode</string>
  <key>CFBundleIdentifier</key><string>dev.forgecode.ide</string>
  <key>CFBundleExecutable</key><string>ForgeCode</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
rm -rf "$work"
echo "Built $out"
