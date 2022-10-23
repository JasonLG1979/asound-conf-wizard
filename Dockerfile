FROM debian:stable

ENV INSIDE_DOCKER_CONTAINER 1

RUN dpkg --add-architecture arm64 \
    && dpkg --add-architecture armhf

RUN apt-get update \
    && apt-get -y upgrade \
    && apt-get install -y --no-install-recommends \
        build-essential \
        libasound2-dev \
        crossbuild-essential-arm64 \
        libasound2-dev:arm64 \
        crossbuild-essential-armhf \
        libasound2-dev:armhf \
        pkg-config \
        ca-certificates \
        curl \
        dpkg-dev \
        liblzma-dev \
    && rm -rf /var/lib/apt/lists/*

ENV PKG_CONFIG_ALLOW_CROSS 1
ENV PKG_CONFIG_PATH "/usr/lib/arm-linux-gnueabihf/pkgconfig"

RUN mkdir /build

RUN curl https://sh.rustup.rs -sSf | sh -s -- -y
ENV PATH "/root/.cargo/bin/:$PATH"
ENV CARGO_INSTALL_ROOT "/root/.cargo"
ENV CARGO_TARGET_DIR "/build"
ENV CARGO_HOME "/build/cache"

RUN rustup target add aarch64-unknown-linux-gnu \
    && rustup target add armv7-unknown-linux-gnueabihf

RUN mkdir /.cargo

RUN cargo install cargo-deb

RUN echo '[target.aarch64-unknown-linux-gnu]\nlinker = "aarch64-linux-gnu-gcc"' > /.cargo/config \
    && echo '[target.armv7-unknown-linux-gnueabihf]\nlinker = "arm-linux-gnueabihf-gcc"' >> /.cargo/config
