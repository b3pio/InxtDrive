#!/usr/bin/env bash
# Assembles InxtDrive-<version>-x86_64.AppImage from a release build.
# Usage: make-appimage.sh <version> <output dir>   (run from the repo root)
#
# WebKitGTK starts helper processes (WebKitWebProcess, WebKitNetworkProcess)
# from a path compiled into libwebkit2gtk. The bundled library is patched to
# look for them relative to the working directory, and AppRun changes into
# $APPDIR/usr first, so the bundled helpers are used instead of whatever
# WebKit version the host has.
set -euo pipefail

version="$1"
out="$2"
src="$(pwd)"
bin="${CARGO_TARGET_DIR:-$src/target}/release"
work="${CARGO_TARGET_DIR:-$src/target}/appimage"
appdir="$work/AppDir"
tools="$work/tools"
export APPIMAGE_EXTRACT_AND_RUN=1
export ARCH=x86_64

mkdir -p "$tools"
fetch() {
  [ -s "$tools/$1" ] || curl -sSfL -o "$tools/$1" "$2"
  chmod +x "$tools/$1"
}
fetch linuxdeploy https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
fetch linuxdeploy-plugin-gtk.sh https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh
fetch appimagetool https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
export PATH="$tools:$PATH"

rm -rf "$appdir"
install -Dm755 "$bin/inxtdrive" "$appdir/usr/bin/inxtdrive"
install -Dm755 "$bin/inxt" "$appdir/usr/bin/inxt"
install -Dm644 packaging/linux/inxtdrive.desktop "$appdir/usr/share/applications/inxtdrive.desktop"
install -Dm644 assets/inxtdrive-256.png "$appdir/usr/share/icons/hicolor/256x256/apps/inxtdrive.png"
install -Dm644 assets/inxtdrive.svg "$appdir/usr/share/icons/hicolor/scalable/apps/inxtdrive.svg"

multiarch="$(gcc -print-multiarch)"
webkit_dir="/usr/lib/$multiarch/webkit2gtk-4.1"
mkdir -p "$appdir/usr/lib/$multiarch"
cp -a "$webkit_dir" "$appdir/usr/lib/$multiarch/"

# The tray library is loaded with dlopen, so linuxdeploy cannot find it alone.
indicator="$(ls /usr/lib/"$multiarch"/libayatana-appindicator3.so.1 | head -n1)"

DEPLOY_GTK_VERSION=3 linuxdeploy \
  --appdir "$appdir" \
  --plugin gtk \
  --executable "$appdir/usr/bin/inxtdrive" \
  --executable "$appdir/usr/bin/inxt" \
  --executable "$appdir/usr/lib/$multiarch/webkit2gtk-4.1/WebKitWebProcess" \
  --executable "$appdir/usr/lib/$multiarch/webkit2gtk-4.1/WebKitNetworkProcess" \
  --library "$indicator" \
  --desktop-file "$appdir/usr/share/applications/inxtdrive.desktop" \
  --icon-file "$appdir/usr/share/icons/hicolor/256x256/apps/inxtdrive.png"

# "/usr" and "././" have the same length, so the binary keeps its layout.
lib="$(ls "$appdir"/usr/lib/libwebkit2gtk-4.1.so.0* | head -n1)"
sed -i -e "s|/usr/lib/$multiarch/webkit2gtk-4.1|././/lib/$multiarch/webkit2gtk-4.1|g" "$lib"

rm -f "$appdir/AppRun"
cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export APPDIR="${APPDIR:-$HERE}"
for hook in "$HERE"/apprun-hooks/*.sh; do
  [ -f "$hook" ] && . "$hook"
done
# Relative paths given on the command line are resolved against this.
export INXT_OWD="${OWD:-$PWD}"
cd "$HERE/usr" || exit 1
exec "$HERE/usr/bin/inxtdrive" "$@"
EOF
chmod +x "$appdir/AppRun"

target="$out/InxtDrive-$version-x86_64.AppImage"
appimagetool --no-appstream "$appdir" "$target"
echo "AppImage: $target"
