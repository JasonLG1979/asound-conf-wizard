#!/bin/sh

if [ "$INSIDE_DOCKER_CONTAINER" != "1" ]; then
	echo "Must be run in docker container"
	exit 1
fi

echo 'Building in docker container'

set -e
cd /mnt/asound-conf-wizard

# Build asound-conf-wizard
cargo-deb --profile default --target "$BUILD_TARGET" -- --jobs "$(nproc)"

cd /build/"$BUILD_TARGET"/debian

AWIZ_DEB_PKG_NAME=$(ls -1 -- *.deb)

cp -v "$AWIZ_DEB_PKG_NAME" /mnt/asound-conf-wizard

echo "asound-conf-wizard package built as $AWIZ_DEB_PKG_NAME"

# Perm fixup. Not needed on macOS, but is on Linux
chown -R "$PERMFIX_UID:$PERMFIX_GID" /mnt/asound-conf-wizard 2>/dev/null || true
