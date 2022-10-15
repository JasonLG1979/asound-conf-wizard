.PHONY: all armhf arm64 amd64 clean distclean
.DEFAULT_GOAL := all

armhf:
	docker build -t asound-conf-wizard .
	docker run \
			--rm \
			--volume "$(CURDIR):/mnt/asound-conf-wizard" \
			--env PERMFIX_UID="$$(id -u)" \
			--env PERMFIX_GID="$$(id -g)" \
			--env BUILD_TARGET="armv7-unknown-linux-gnueabihf" \
		asound-conf-wizard /mnt/asound-conf-wizard/build.sh

arm64:
	docker build -t asound-conf-wizard .
	docker run \
			--rm \
			--volume "$(CURDIR):/mnt/asound-conf-wizard" \
			--env PERMFIX_UID="$$(id -u)" \
			--env PERMFIX_GID="$$(id -g)" \
			--env BUILD_TARGET="aarch64-unknown-linux-gnu" \
		asound-conf-wizard /mnt/asound-conf-wizard/build.sh

amd64:
	docker build -t asound-conf-wizard .
	docker run \
			--rm \
			--volume "$(CURDIR):/mnt/asound-conf-wizard" \
			--env PERMFIX_UID="$$(id -u)" \
			--env PERMFIX_GID="$$(id -g)" \
			--env BUILD_TARGET="x86_64-unknown-linux-gnu" \
		asound-conf-wizard /mnt/asound-conf-wizard/build.sh

all: armhf arm64 amd64

clean:
	rm -rf *.deb

distclean: clean
	docker rmi -f asound-conf-wizard || true
