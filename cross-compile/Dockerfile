# Cross compilation environment for r357
# Build the docker image from the root of the project with the following command :
# $ docker build -t r357-cross -f cross-compile/Dockerfile .
#
# The resulting image can be used to build r357 for linux x86_64, armhf, armel, aarch64
# $ docker run -v /tmp/r357-build:/build r357-cross
#
# The compiled binaries will be located in /tmp/r357-build
#
# If only one architecture is desired, cargo can be invoked directly with the appropriate options :
# $ docker run -v /tmp/r357-build:/build r357-cross cargo build --release --no-default-features
# $ docker run -v /tmp/r357-build:/build r357-cross cargo build --release --target arm-unknown-linux-gnueabihf
# $ docker run -v /tmp/r357-build:/build r357-cross cargo build --release --target arm-unknown-linux-gnueabi
# $ docker run -v /tmp/r357-build:/build r357-cross cargo build --release --target aarch64-unknown-linux-gnu

FROM rust:bookworm

RUN dpkg --add-architecture arm64 && \
	dpkg --add-architecture armhf && \
	dpkg --add-architecture armel && \
	apt-get update && \
	apt-get install -y \
	build-essential \
	cmake \
	crossbuild-essential-arm64 \
	crossbuild-essential-armel \
	crossbuild-essential-armhf \
	curl \
	git \
	libasound2-dev \
	libasound2-dev:arm64 \
	libasound2-dev:armel \
	libasound2-dev:armhf \
	libclang-dev \
	libpulse0 \
	libpulse0:arm64 \
	libpulse0:armel \
	libpulse0:armhf \
    libpulse-dev:arm64 \
    libpulse-dev:armel \
    libpulse-dev:armhf \
	libssl-dev \
	libssl-dev:arm64 \
	libssl-dev:armel \
	libssl-dev:armhf \
	pkg-config

ENV PATH="/root/.cargo/bin/:${PATH}"
RUN rustup default stable && \
	rustup target add aarch64-unknown-linux-gnu && \
	rustup target add arm-unknown-linux-gnueabi && \
	rustup target add arm-unknown-linux-gnueabihf
RUN cargo install bindgen-cli && \
	mkdir /.cargo && \
	echo '[target.aarch64-unknown-linux-gnu]\nlinker = "aarch64-linux-gnu-gcc"\nrustflags = ["-C", "link-args=--verbose"]' > /.cargo/config && \
	echo '[target.arm-unknown-linux-gnueabihf]\nlinker = "arm-linux-gnueabihf-gcc"' >> /.cargo/config && \
	echo '[target.arm-unknown-linux-gnueabi]\nlinker = "arm-linux-gnueabi-gcc"' >> /.cargo/config

ENV CARGO_TARGET_DIR=/build
ENV CARGO_HOME=/build/cache
ENV PKG_CONFIG_ALLOW_CROSS=1
ENV PKG_CONFIG_PATH_aarch64-unknown-linux-gnu=/usr/lib/aarch64-linux-gnu/pkgconfig/
ENV PKG_CONFIG_PATH_arm-unknown-linux-gnueabihf=/usr/lib/arm-linux-gnueabihf/pkgconfig/
ENV PKG_CONFIG_PATH_arm-unknown-linux-gnueabi=/usr/lib/arm-linux-gnueabi/pkgconfig/

ADD . /src
WORKDIR /src
CMD ["/src/contrib/docker-build.sh"]
