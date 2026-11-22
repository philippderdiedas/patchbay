# SPDX-License-Identifier: GPL-3.0-or-later
# Asterisk 24 LTS from source, only the modules the PBX needs. The image carries the complete,
# site-neutral configuration; a site only adds env, secrets and data mounts (deploy/compose.yml).
# Build context: asterisk/ (deploy/build.sh).
ARG DEBIAN=trixie-slim
ARG ASTERISK_VERSION=24.0.0

FROM debian:${DEBIAN} AS build
ARG ASTERISK_VERSION
ARG ASTERISK_SHA256=ece42a75bc965180007c56b03f2fd1dc8484eb84fc4cddc090db0ee5d5da1af9

RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl bzip2 patch pkg-config python3 \
      libedit-dev libjansson-dev libssl-dev libsqlite3-dev uuid-dev libxml2-dev \
      libxslt1-dev libcurl4-openssl-dev unixodbc-dev libsrtp2-dev libspeexdsp-dev \
      libopus-dev libogg-dev libvorbis-dev libgsm1-dev libpq-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src
RUN curl -fsSLO https://downloads.asterisk.org/pub/telephony/asterisk/releases/asterisk-${ASTERISK_VERSION}.tar.gz \
 && echo "${ASTERISK_SHA256}  asterisk-${ASTERISK_VERSION}.tar.gz" | sha256sum -c - \
 && tar xzf asterisk-${ASTERISK_VERSION}.tar.gz && mv asterisk-${ASTERISK_VERSION} asterisk

WORKDIR /usr/src/asterisk
# open-source Opus transcoder (asterisk/opus/README.md)
COPY opus/codec_opus_open_source.c opus/ex_opus.h codecs/
RUN ./configure --with-pjproject-bundled --with-jansson-bundled=no --libdir=/usr/lib \
      --prefix=/usr --sysconfdir=/etc --localstatedir=/var \
 && make menuselect.makeopts \
 && menuselect/menuselect \
      --disable-category MENUSELECT_ADDONS \
      --disable app_voicemail --disable app_voicemail_imap --disable app_voicemail_odbc \
      --disable chan_unistim --disable res_config_ldap \
      --disable BUILD_NATIVE \
      --enable res_config_odbc --enable res_odbc --enable func_odbc --enable cdr_adaptive_odbc \
      --enable res_mwi_external --enable res_ari_mailboxes --enable res_mwi_external_ami \
      --enable chan_iax2 --enable func_curl --enable res_curl --enable app_queue \
      --enable codec_g722 --enable codec_opus_open_source --enable format_ogg_vorbis --enable format_wav --enable res_srtp \
      --enable CORE-SOUNDS-EN-WAV --enable CORE-SOUNDS-EN-G722 \
      --enable MOH-OPSOUND-WAV \
      menuselect.makeopts \
 && make -j"$(nproc)" \
 && make install DESTDIR=/out \
 && make samples DESTDIR=/out >/dev/null \
 && rm -rf /out/etc/asterisk
# /var/run is a symlink to /run in the target image
RUN mkdir -p /out/run && cp -a /out/var/run/. /out/run/ && rm -rf /out/var/run

# German prompts Debian's pack lacks (speaking clock …), spoken by Piper TTS with the free voice
# "Thorsten" (CC0) from asterisk/tts/prompts-de.txt; installed as sounds/de/pbx-tts/<name>.wav.
FROM debian:${DEBIAN} AS tts
ARG PIPER_URL=https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_x86_64.tar.gz
ARG PIPER_SHA256=a50cb45f355b7af1f6d758c1b360717877ba0a398cc8cbe6d2a7a3a26e225992
ARG VOICE_URL=https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/de/de_DE/thorsten/medium/de_DE-thorsten-medium.onnx
ARG VOICE_SHA256=7e64762d8e5118bb578f2eea6207e1a35a8e0c30595010b666f983fc87bb7819
ARG VOICE_JSON_SHA256=974adee790533adb273a1ac88f49027d2a1b8f0f2cf4905954a4791e79264e85
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl sox && rm -rf /var/lib/apt/lists/*
WORKDIR /tts
RUN curl -fsSL -o piper.tar.gz "$PIPER_URL" && echo "$PIPER_SHA256  piper.tar.gz" | sha256sum -c - \
 && curl -fsSL -o voice.onnx "$VOICE_URL" && echo "$VOICE_SHA256  voice.onnx" | sha256sum -c - \
 && curl -fsSL -o voice.onnx.json "$VOICE_URL.json" && echo "$VOICE_JSON_SHA256  voice.onnx.json" | sha256sum -c - \
 && tar xzf piper.tar.gz
COPY tts/prompts-de.txt .
RUN mkdir -p /out/de/pbx-tts \
 && grep -v '^#' prompts-de.txt | while IFS='|' read -r name text; do \
      [ -n "$name" ] || continue; \
      echo "$text" | ./piper/piper --model voice.onnx --config voice.onnx.json --output_file /tmp/p.wav 2>/dev/null \
      && sox /tmp/p.wav -r 8000 -c 1 -b 16 "/out/de/pbx-tts/$name.wav" || exit 1; \
    done \
 && test "$(ls /out/de/pbx-tts | wc -l)" -eq "$(grep -cv '^#' prompts-de.txt)"

FROM debian:${DEBIAN}
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates libedit2 libjansson4 libssl3t64 libsqlite3-0 libuuid1 libxml2 libxslt1.1 \
      libcurl4t64 unixodbc odbc-postgresql libsrtp2-1 libspeexdsp1 libopus0 libogg0 libvorbis0a \
      libvorbisenc2 libvorbisfile3 libgsm1 libpq5 curl sox tini asterisk-prompt-de \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /var/lib/asterisk --shell /usr/sbin/nologin asterisk
COPY --from=build /out/ /
# German system prompts from Debian (asterisk-prompt-de) under the Asterisk sounds path
RUN ln -s /usr/share/asterisk/sounds/de /var/lib/asterisk/sounds/de
COPY --from=tts /out/de/pbx-tts /usr/share/asterisk/sounds/de/pbx-tts
RUN mkdir -p /etc/asterisk /var/spool/pbx/vm /var/lib/asterisk/db \
 && chown -R asterisk:asterisk /var/lib/asterisk /var/spool/asterisk /var/log/asterisk /var/run/asterisk /var/spool/pbx
COPY odbcinst.ini /etc/odbcinst.ini
# the driver lives in the multiarch directory (x86_64-linux-gnu, aarch64-linux-gnu, ...)
RUN drv=$(echo /usr/lib/*/odbc/psqlodbcw.so) && test -f "$drv" \
 && sed -i "s|^Driver .*|Driver      = $drv|" /etc/odbcinst.ini && grep -q "$drv" /etc/odbcinst.ini
# Media: /media is linked to $PBX_STATE_DIR/media at start (site-config.sh; written by patchbay-web,
# Asterisk only reads it); the paths the configuration and the database views use point into it.
#   sounds/pbx    announcements (sounds.file_path, Playback(pbx/<id>))
#   moh-apps      hold music classes (musiconhold.conf [default], musiconhold view: app<id>, pl<id>)
#   moh-default   initial hold music seed (patchbay-web imports it; not read by Asterisk itself)
RUN ln -s /media/asterisk /var/lib/asterisk/sounds/pbx \
 && ln -s /media/moh /var/lib/asterisk/moh-apps \
 && ln -s /media/moh-default /var/lib/asterisk/moh-default

# Configuration last: changing it does not rebuild Asterisk. Prepared at start by site-config.sh (not in
# the image): /etc/asterisk-site (tmpfs, rendered), /etc/asterisk-secrets (copies), /etc/asterisk-keys and
# /etc/asterisk-generated (links into the state directory; generated files are written by patchbay-web).
COPY odbc.ini /etc/odbc.ini
COPY etc/ /etc/asterisk/
COPY bin/site-config.sh /usr/local/bin/site-config.sh
RUN chmod 755 /usr/local/bin/site-config.sh && chmod -R a+rX,go-w /etc/asterisk /etc/odbc.ini

ARG ASTERISK_VERSION
ARG PBX_VERSION=dev
LABEL org.opencontainers.image.title="patchbay-asterisk" \
      org.opencontainers.image.version="${PBX_VERSION}" \
      pbx.asterisk.version="${ASTERISK_VERSION}"
ENTRYPOINT ["/usr/bin/tini", "--"]
# renders the site files (ACL, transports, globals, credentials) from env and secrets, then starts Asterisk
CMD ["/bin/sh", "/usr/local/bin/site-config.sh", "/usr/sbin/asterisk", "-f", "-U", "asterisk", "-G", "asterisk", "-vvv"]
