#!/bin/sh
set -eu
# This script updates the version number for
# the addon based on the current commit timestamp
TAG_NAME=${TAG_NAME:-$(git -c "core.abbrev=8" show -s "--format=%cd-%h" "--date=format:%Y.%m.%d")}
case "$TAG_NAME" in
    *[!a-zA-Z0-9_.-]*) echo "Invalid image tag" >&2; exit 1 ;;
esac
sed -i "s/version:.*/version: \"$TAG_NAME\"/" addon/config.yaml
sed -i "s/^ARG GOVEE_VERSION=.*/ARG GOVEE_VERSION=$TAG_NAME/" addon/Dockerfile
