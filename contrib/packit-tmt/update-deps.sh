#!/bin/bash
# Set podman-next COPR repository priority to 5 (lower than Packit PR COPR which is priority=1)
# This ensures the PR build of conmon-v3 takes precedence over podman-next's main-branch build

COPR_REPO_FILE="/etc/yum.repos.d/*podman-next*.repo"
if compgen -G $COPR_REPO_FILE > /dev/null; then
    sed -i '/^priority=/d; $apriority=5' $COPR_REPO_FILE
fi
dnf -y upgrade --allowerasing
