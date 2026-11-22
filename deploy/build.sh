#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Builds the images patchbay-asterisk and patchbay-web from this checkout.
#   deploy/build.sh [--context <docker-context>] [--tag <tag>] [--only asterisk|web] [-- <docker build args>]
# Tag: `git describe --always --dirty` (fallback "dev"), both images are also tagged :latest.
# Compose uses ${PBX_IMAGE_PREFIX}pbx-*:${PBX_VERSION:-latest}: set PBX_VERSION=<tag> (.env next to
# compose.yml) to pin a version (rollback = set the previous tag, `docker compose up -d`).
# --context builds with that Docker context (e.g. directly on the PBX host's daemon).
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DOCKER=(docker)
TAG=
ONLY=
while [ $# -gt 0 ]; do
    case "$1" in
        --context) DOCKER=(docker --context "${2:?--context needs a name}"); shift ;;
        --tag) TAG=${2:?--tag needs a value}; shift ;;
        --only) ONLY=${2:?--only needs asterisk or web}; shift ;;
        --) shift; break ;;
        -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
        *) echo "unknown option $1" >&2; exit 1 ;;
    esac
    shift
done
case "$ONLY" in ''|asterisk|web) ;; *) echo "--only: asterisk or web" >&2; exit 1 ;; esac
[ -n "$TAG" ] || TAG=$(git -C "$ROOT" describe --always --dirty 2>/dev/null || true)
[ -n "$TAG" ] || TAG=dev
# Docker tags: [A-Za-z0-9_.-], at most 128 characters
[[ $TAG =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]] || { echo "invalid tag: $TAG" >&2; exit 1; }

if [ "$ONLY" != web ]; then
    echo "== patchbay-asterisk:$TAG"
    "${DOCKER[@]}" build --build-arg "PBX_VERSION=$TAG" -t "patchbay-asterisk:$TAG" -t patchbay-asterisk:latest "$@" "$ROOT/asterisk"
fi
if [ "$ONLY" != asterisk ]; then
    echo "== patchbay-web:$TAG"
    "${DOCKER[@]}" build --build-arg "PBX_VERSION=$TAG" -t "patchbay-web:$TAG" -t patchbay-web:latest "$@" -f "$ROOT/patchbay-web/Dockerfile" "$ROOT"
fi
# PBX_VERSION is shared by both images: with --only, the other image's current :latest gets the tag too
case "$ONLY" in
    web) "${DOCKER[@]}" tag patchbay-asterisk:latest "patchbay-asterisk:$TAG" ;;
    asterisk) "${DOCKER[@]}" tag patchbay-web:latest "patchbay-web:$TAG" ;;
esac
echo "patchbay-asterisk:$TAG and patchbay-web:$TAG ready (also :latest)"
