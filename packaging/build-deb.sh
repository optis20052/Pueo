#!/bin/sh
# Build dist/pueo_<version>_<arch>.deb. Installing it with apt (or App Center) also
# installs everything Pueo needs at runtime: GTK and its libraries, ssh for tunnels,
# and the PostgreSQL / MariaDB client tools used for backup and restore.
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
arch=$(dpkg --print-architecture)
maintainer=${DEBMAINTAINER:-"Pueo Developers <pueo@users.noreply.github.com>"}

cargo build --release

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root="$work/pueo_${version}_${arch}"
install -Dm755 target/release/pueo "$root/usr/bin/pueo"
strip --strip-unneeded "$root/usr/bin/pueo"
install -Dm644 data/io.github.pueo.Pueo.desktop "$root/usr/share/applications/io.github.pueo.Pueo.desktop"
install -Dm644 data/icons/hicolor/scalable/apps/io.github.pueo.Pueo.svg \
  "$root/usr/share/icons/hicolor/scalable/apps/io.github.pueo.Pueo.svg"

# The shared libraries the binary links against, as package names.
mkdir -p "$work/debian"
printf 'Source: pueo\n\nPackage: pueo\nArchitecture: any\n' > "$work/debian/control"
libs=$(cd "$work" && dpkg-shlibdeps -O "$root/usr/bin/pueo" | sed 's/^shlibs:Depends=//')

mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: pueo
Version: $version
Architecture: $arch
Maintainer: $maintainer
Section: database
Priority: optional
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Depends: $libs, openssh-client, postgresql-client, mariadb-client | default-mysql-client
Recommends: gnome-keyring
Description: Native database client for GNOME
 Browse and edit PostgreSQL, MySQL / MariaDB, SQLite and Redis databases.
 Includes the client tools used to back up and restore whole databases.
CONTROL

mkdir -p dist
dpkg-deb --root-owner-group --build "$root" dist/
