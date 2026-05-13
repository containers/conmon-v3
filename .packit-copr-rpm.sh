#!/usr/bin/env bash

# Custom spec handling for Packit's `fix-spec-file` action (.packit.yaml).
#
# Mirrors containers/netavark `.packit-copr-rpm.sh`:
# https://github.com/containers/netavark/blob/main/.packit-copr-rpm.sh
#
# - Version / Release / Source0 / %autosetup from Cargo.toml + git HEAD
# - Drops Source1 (vendor tarball); sets %%packit_no_vendor_tarball so %%prep skips `tar fx`

set -uexo pipefail

PACKAGE=conmon-v3
SPEC_FILE=rpm/${PACKAGE}.spec

VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)

# Source tarball from current HEAD (matches %autosetup -n below)
git-archive-all -C "$(git rev-parse --show-toplevel)" \
	--prefix="${PACKAGE}-${VERSION}/" \
	"rpm/${PACKAGE}-${VERSION}.tar.gz"

# Spec edits for Copr SRPM
sed -i "s/^%global upstream_version.*/%global upstream_version ${VERSION}/" "${SPEC_FILE}"
sed -i "s/^Release:.*/Release: ${PACKIT_RPMSPEC_RELEASE}%{?dist}/" "${SPEC_FILE}"
sed -i "s/^Source0:.*\\.tar\\.gz/Source0: ${PACKAGE}-${VERSION}.tar.gz/" "${SPEC_FILE}"
sed -i "/^Source1/d" "${SPEC_FILE}"
sed -i '/^Name:/a %global packit_no_vendor_tarball 1' "${SPEC_FILE}"
sed -i "s/^%autosetup.*/%autosetup -Sgit -n ${PACKAGE}-${VERSION} -p1/" "${SPEC_FILE}"
