#!/bin/sh
# Build dist/voicecode-<version>-x86_64.AppImage. Run: scripts/build-appimage.sh
set -eu
cd "$(dirname "$0")/.."  # project root

# pyproject.toml is the single source of the version.
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' pyproject.toml | head -n 1)
[ -n "$VERSION" ] || { echo "No version found in pyproject.toml" >&2; exit 1; }
OUT=dist/voicecode-$VERSION-x86_64.AppImage
STABLE=dist/voicecode-x86_64.AppImage  # fixed name used by the autostart .desktop

APPIMAGETOOL_VERSION=1.9.0
TOOL=build/tools/appimagetool-$APPIMAGETOOL_VERSION
APPDIR=build/AppDir

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/lib/voicecode" build/tools dist
cp packaging/AppRun packaging/voicecode.desktop packaging/voicecode.svg "$APPDIR/"
chmod +x "$APPDIR/AppRun"
echo "X-AppImage-Version=$VERSION" >> "$APPDIR/voicecode.desktop"
# Installs the voicecode package and its deps, for the same Python the AppImage runs with.
uv pip install --quiet --python /usr/bin/python3 --target "$APPDIR/usr/lib/voicecode/site-packages" .

if [ ! -x "$TOOL" ]; then
    curl -fSL -o "$TOOL" \
        "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-x86_64.AppImage"
    chmod +x "$TOOL"
fi
ARCH=x86_64 VERSION="$VERSION" "$TOOL" --no-appstream "$APPDIR" "$OUT"
# Symlink where the filesystem allows it, otherwise a copy (e.g. exFAT/NTFS drives).
rm -f "$STABLE"
ln -s "$(basename "$OUT")" "$STABLE" 2>/dev/null || cp "$OUT" "$STABLE"
echo "Built $OUT (also at $STABLE)"
