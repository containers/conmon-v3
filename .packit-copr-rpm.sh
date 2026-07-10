#!/usr/bin/env bash

# Custom spec handling for Packit's `fix-spec-file` action (.packit.yaml).
#
# Copr-only: sync Version/Release/commit from HEAD.
# Prefer forge HTTPS Source0 when the commit is on the remote; otherwise fall back
# to a local git-archive-all tarball. Drops Source1 (vendor tarball).

set -uexo pipefail

PACKAGE=conmon-v3
SPEC_FILE=rpm/${PACKAGE}.spec

VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
COMMIT=$(git rev-parse HEAD)
FORGEURL=$(awk '/^%global forgeurl / { print $3 }' "${SPEC_FILE}")
ARCHIVE_URL="${FORGEURL}/archive/${COMMIT}.tar.gz"

sed -i "s/^%global upstream_version.*/%global upstream_version ${VERSION}/" "${SPEC_FILE}"
sed -i "s/^%global commit.*/%global commit ${COMMIT}/" "${SPEC_FILE}"
sed -i "s/^Release:.*/Release: ${PACKIT_RPMSPEC_RELEASE}%{?dist}/" "${SPEC_FILE}"

if ! grep -q '^%global packit_no_vendor_tarball' "${SPEC_FILE}"; then
	sed -i '/^Name:/a %global packit_no_vendor_tarball 1' "${SPEC_FILE}"
fi
sed -i "/^Source1/d" "${SPEC_FILE}"

if curl -fsSIL --retry 2 "${ARCHIVE_URL}" >/dev/null; then
	echo "Using forge Source0: ${ARCHIVE_URL}"
else
	echo "Commit ${COMMIT} not on forge; falling back to git-archive-all"
	TARBALL="rpm/${PACKAGE}-${VERSION}.tar.gz"
	git-archive-all -C "$(git rev-parse --show-toplevel)" \
		--prefix="${PACKAGE}-${VERSION}/" \
		"${TARBALL}"
	sed -i "s|^Source0:.*|Source0: ${PACKAGE}-${VERSION}.tar.gz|" "${SPEC_FILE}"
	sed -i "s|^%autosetup.*|%autosetup -Sgit -n ${PACKAGE}-${VERSION} -p1|" "${SPEC_FILE}"
fi
