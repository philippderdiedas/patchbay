# SPDX-License-Identifier: GPL-3.0-or-later
# Build context is the repository root (patchbay-web depends on ../patchbay-notify, build.rs embeds
# ../db/migrations for `patchbay-web migrate`). Usually built by deploy/build.sh, by hand:
#   docker build -t patchbay-web:latest -f patchbay-web/Dockerfile .
FROM rust:1-trixie AS build
WORKDIR /src
# Dependencies in their own layer: rebuilt only when Cargo.toml/Cargo.lock change. Plain layer
# caching (no RUN --mount), so it also works with kaniko --cache in GitLab CI.
COPY patchbay-web/Cargo.toml patchbay-web/Cargo.lock patchbay-web/
COPY patchbay-notify/Cargo.toml patchbay-notify/
RUN mkdir -p patchbay-web/src patchbay-notify/src && echo 'fn main() {}' > patchbay-web/src/main.rs && touch patchbay-notify/src/lib.rs \
    && cd patchbay-web && cargo build --release --locked \
    && rm -rf src ../patchbay-notify/src target/release/patchbay-web
COPY patchbay-notify/src patchbay-notify/src
COPY db/migrations db/migrations
COPY patchbay-web/build.rs patchbay-web/askama.toml patchbay-web/
COPY patchbay-web/src patchbay-web/src
COPY patchbay-web/templates patchbay-web/templates
COPY patchbay-web/static patchbay-web/static
# touch: the copied sources can be older than the dummy build, cargo would keep the dummy binary.
# The size check makes sure that never ships.
RUN touch patchbay-web/src/main.rs patchbay-notify/src/lib.rs && cd patchbay-web && cargo build --release --locked \
    && test "$(stat -c %s target/release/patchbay-web)" -gt 5000000

# Initial company hold music (Opsound, CC BY-SA, same files Asterisk ships): patchbay-web imports it into
# the audio library on a new site (moh.rs), unless the site put its own files into media/moh-default.
FROM debian:trixie-slim AS moh
ARG MOH_URL=https://downloads.asterisk.org/pub/telephony/sounds/releases/asterisk-moh-opsound-wav-2.03.tar.gz
ARG MOH_SHA256=449fb810d16502c3052fedf02f7e77b36206ac5a145f3dacf4177843a2fcb538
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL -o /moh.tar.gz "$MOH_URL" && echo "$MOH_SHA256  /moh.tar.gz" | sha256sum -c - \
    && mkdir -p /out/moh-default && tar xzf /moh.tar.gz -C /out/moh-default

FROM debian:trixie-slim
# openssl: secrets and the interim certificate in patchbay-init (also pulled in by ca-certificates)
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates openssl libcap2-bin ffmpeg \
    && rm -rf /var/lib/apt/lists/*
COPY --from=moh /out/moh-default /usr/share/pbx/moh-default
# one-shot state setup (as root, called by patchbay-start): secrets, directories, pg_hba.conf
COPY patchbay-web/bin/patchbay-init.sh /usr/local/bin/patchbay-init
# entrypoint: as root with the state directory at /state it runs init + migrations, then patchbay-web
# unprivileged (see deploy/compose.yml); with arguments or as non-root it runs patchbay-web directly
COPY patchbay-web/bin/patchbay-start.sh /usr/local/bin/patchbay-start
RUN chmod 755 /usr/local/bin/patchbay-init /usr/local/bin/patchbay-start
COPY --from=build /src/patchbay-web/target/release/patchbay-web /usr/local/bin/patchbay-web
# port 443 without root (Compose grants NET_BIND_SERVICE)
RUN setcap cap_net_bind_service=+ep /usr/local/bin/patchbay-web
# last: the version changes on every build, the layers above stay cached
ARG PBX_VERSION=dev
LABEL org.opencontainers.image.title="patchbay-web" org.opencontainers.image.version="${PBX_VERSION}"
# shown in the web UI footer (admins), the monitoring and the log (config::build_version)
ENV PBX_BUILD=${PBX_VERSION}
# Secret files (patchbay-start copies them to /run/patchbay-web), also for
# `docker compose exec patchbay-web patchbay-web …` (set-password, mint-session); override the
# variables to use other paths.
ENV PBX_DATABASE_URL_FILE=/run/patchbay-web/database_url \
    PBX_COOKIE_KEY_FILE=/run/patchbay-web/cookie_key \
    PBX_AMI_SECRET_FILE=/run/patchbay-web/ami_pw \
    PBX_ARI_SECRET_FILE=/run/patchbay-web/ari_pw \
    PBX_SECRET_KEY_FILE=/run/patchbay-web/secret_key \
    PBX_INTERNAL_KEY_FILE=/run/patchbay-web/internal_key
USER 65534:65534
# ready = listening after migrations (written by patchbay-web)
HEALTHCHECK --interval=5s --timeout=3s --start-period=180s --retries=3 CMD ["test", "-f", "/tmp/pbx-ready"]
ENTRYPOINT ["/usr/local/bin/patchbay-start"]
