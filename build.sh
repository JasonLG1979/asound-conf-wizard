#!/bin/sh

if [ "$INSIDE_DOCKER_CONTAINER" != "1" ]; then
    echo "Must be run in docker container"
    exit 1
fi

echo 'Building in docker container'

set -e
cd /mnt/asound-conf-wizard

# Build asound-conf-wizard
/build/cache/bin/cargo-deb --profile default --target $BUILD_TARGET

cp -v /build/$BUILD_TARGET/debian/*.deb /mnt/asound-conf-wizard

# Perm fixup. Not needed on macOS, but is on Linux
chown -R "$PERMFIX_UID:$PERMFIX_GID" /mnt/asound-conf-wizard 2> /dev/null || true
