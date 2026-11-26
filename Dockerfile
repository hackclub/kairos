# Builds the server from source, for hosts that build straight from the repo. deploy/Dockerfile is the other way:
# it packages a binary built on the host.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p kairos-server -p kairos-release

# The client release this server hands out for installs and updates. Its signature is checked before it goes in.
ARG RELEASE_REPO=hackclub/kairos
ARG RELEASE_TAG=v0.1.23
RUN GH_REPO=$RELEASE_REPO KAIROS_RELEASE=target/release/kairos-release KAIROS_DOWNLOADS=/downloads \
    sh deploy/publish-release.sh $RELEASE_TAG

FROM debian:bookworm-slim
RUN useradd -r -u 10001 kairos
COPY --from=build /src/target/release/kairos-server /usr/local/bin/kairos-server
COPY --from=build /downloads /downloads
USER kairos
ENV BIND=0.0.0.0:8787 KAIROS_DOWNLOADS=/downloads
EXPOSE 8787
CMD ["kairos-server"]
