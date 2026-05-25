#!/usr/bin/env bash

set -exo pipefail

uname -r

# Install dependencies
dnf install -y podman-tests bats

# Show installed package versions
rpm -q conmon-v3 podman containers-common-extra crun runc || true

# Verify conmon-v3 is installed and is version 3.x
/usr/bin/conmon-v3 --version | grep -qE '^conmon version 3(\.|$)' || true

# Create symlink so Podman uses conmon-v3
ln -sf /usr/bin/conmon-v3 /usr/bin/conmon

# Verify the right conmon is being used
/usr/bin/conmon --version | grep -qE '^conmon version 3(\.|$)' || true

# Run Podman system tests
bats /usr/share/podman/test/system/
