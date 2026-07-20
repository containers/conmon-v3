MAKEFILE_PATH := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
BINARY := conmon
CONTAINER_RUNTIME ?= podman
BUILD_DIR ?= .build
TEST_FLAGS ?=
# When CI is set (e.g. in GitHub Actions), keep Cargo.lock fixed (see containers/netavark Makefile).
CARGO ?= cargo $(if $(CI),--locked,)
# Directory for RPM Source1: conmon-v3-v$(version)-vendor.tar.gz (top-level path ./vendor/).
VENDOR_TARBALL_DIR ?= vendor-tarball
PACKAGE_NAME ?= $(shell cargo metadata --no-deps --format-version 1 | jq -r '.packages[2] | [ .name, .version ] | join("-v")')
PREFIX ?= /usr/local
DATADIR ?= ${PREFIX}/share
MANDIR ?= $(DATADIR)/man
CI_TAG ?=
CONMON_V2_DIR ?= conmon-v2
CONMON_V2_URL ?= https://github.com/containers/conmon.git
CONMON_V2_REMOTE ?= conmon-v2
CONMON_V2_BRANCH ?= main

COLOR:=\\033[36m
NOCOLOR:=\\033[0m
WIDTH:=25

all: default

.PHONY: help
help:  ## Display this help.
	@awk \
		-v "col=${COLOR}" -v "nocol=${NOCOLOR}" \
		' \
			BEGIN { \
				FS = ":.*##" ; \
				printf "Usage:\n  make %s<target>%s\n", col, nocol \
			} \
			/^[./a-zA-Z_-]+:.*?##/ { \
				printf "  %s%-${WIDTH}s%s %s\n", col, $$1, nocol, $$2 \
			} \
			/^##@/ { \
				printf "\n%s\n", substr($$0, 5) \
			} \
		' $(MAKEFILE_LIST)

##@ Build targets:

.PHONY: default
default: docs ## Build the conmon binary.
	$(CARGO) build

.PHONY: release
release: docs ## Build the conmon binary in release mode.
	$(CARGO) build --release

.PHONY: release-static
release-static: ## Build the conmon binary in release-static mode.
	RUSTFLAGS="-C target-feature=+crt-static" $(CARGO) build --release --target x86_64-unknown-linux-gnu
	strip -s target/x86_64-unknown-linux-gnu/release/conmon
	ldd target/x86_64-unknown-linux-gnu/release/conmon 2>&1 | grep -qE '(statically linked)|(not a dynamic executable)'

##@ Test targets:

.PHONY: test
test: unit e2e ## Run both `unit` and `e2e` tests

.PHONY: unit
unit: ## Run the unit tests.
	$(CARGO) test --no-fail-fast

.PHONY: e2e
e2e: conmon-v2 ## Run the e2e tests.
	CONMON_BINARY="$(MAKEFILE_PATH)target/debug/conmon" conmon-v2/test/run-tests.sh

.PHONY: .install.fmt
.install.fmt:
	@if ! cargo fmt --version >/dev/null 2>&1; then \
		if command -v rustfmt >/dev/null 2>&1; then \
			mkdir -p ~/.cargo/bin; \
			echo '#!/bin/bash' > ~/.cargo/bin/cargo-fmt; \
			echo 'exec rustfmt "$$@"' >> ~/.cargo/bin/cargo-fmt; \
			chmod +x ~/.cargo/bin/cargo-fmt; \
			export PATH="$$HOME/.cargo/bin:$$PATH"; \
			echo "Created cargo-fmt wrapper in ~/.cargo/bin/"; \
		else \
			echo "Error: rustfmt not found" >&2; \
			exit 1; \
		fi \
	fi

.PHONY: lint
lint: ## Run the linter.
	$(CARGO) fmt && git diff --exit-code
	$(CARGO) clippy --all-targets --all-features -- -D warnings

##@ Vendor / RPM (offline) targets:

.PHONY: vendor
vendor: ## Populate ./vendor for offline builds (see rpm/conmon-v3.spec Source1).
	rm -rf vendor
	$(CARGO) vendor vendor

.PHONY: install.cargo-vendor-filterer
install.cargo-vendor-filterer: ## Install cargo-vendor-filterer (optional smaller vendor tarballs).
	cargo install cargo-vendor-filterer

.PHONY: vendor-tarball
vendor-tarball: vendor ## Write $(VENDOR_TARBALL_DIR)/conmon-v3-v$(version)-vendor.tar.gz for RPM Source1.
	@set -euo pipefail; \
	version="$$(grep '^version' Cargo.toml | head -1 | sed -E 's/^version[[:space:]]*=[[:space:]]*//; s/^\"//; s/\"$$//')"; \
	mkdir -p "$(VENDOR_TARBALL_DIR)"; \
	tar --exclude-vcs -czf "$(VENDOR_TARBALL_DIR)/conmon-v3-v$$version-vendor.tar.gz" vendor; \
	echo "Wrote $(VENDOR_TARBALL_DIR)/conmon-v3-v$$version-vendor.tar.gz"

.PHONY: vendor-tarball-filtered
vendor-tarball-filtered: release install.cargo-vendor-filterer ## Smaller vendor tarball via cargo-vendor-filterer.
	@set -euo pipefail; \
	version="$$(grep '^version' Cargo.toml | head -1 | sed -E 's/^version[[:space:]]*=[[:space:]]*//; s/^\"//; s/\"$$//')"; \
	mkdir -p "$(VENDOR_TARBALL_DIR)"; \
	cargo vendor-filterer --format=tar.gz --prefix vendor/; \
	mv vendor.tar.gz "$(VENDOR_TARBALL_DIR)/conmon-v3-v$$version-vendor.tar.gz"; \
	echo "Wrote $(VENDOR_TARBALL_DIR)/conmon-v3-v$$version-vendor.tar.gz"

##@ Utility targets:

.PHONY: prettier
prettier: ## Prettify supported files.
	$(CONTAINER_RUNTIME) run -it --privileged -v ${PWD}:/w -w /w --entrypoint bash node:latest -c \
		'npm install -g prettier && prettier -w .'

.PHONY: docs
docs: ## Generate man pages.
	$(MAKE) -C docs docs

.PHONY: clean
clean: ## Cleanup the project files.
	rm -rf target/
	rm -rf conmon-v2/
	rm -rf vendor vendor-tarball

.PHONY: install
install: docs ## Install the binary.
	install -d "${DESTDIR}$(PREFIX)/bin"
	@set -eu; \
	bin=""; \
	if [ -x target/rpm/conmon ]; then \
		bin="target/rpm/conmon"; \
	elif [ -x target/release/conmon ]; then \
		bin="target/release/conmon"; \
	elif [ -x target/debug/conmon ]; then \
		bin="target/debug/conmon"; \
	else \
		echo "ERROR: no conmon binary found. Build one of:"; \
		echo "  - make (debug)"; \
		echo "  - make release"; \
		echo "  - rpm build (produces target/rpm/conmon)"; \
		exit 1; \
	fi; \
	install -m 0755 "$$bin" "${DESTDIR}$(PREFIX)/bin/conmon-v3"
	install -d "${DESTDIR}${MANDIR}/man8"
	install -m 0644 docs/conmon.8 "${DESTDIR}${MANDIR}/man8/conmon-v3.8"

.PHONY: conmon-v2
conmon-v2: ## Fetch the conmon-v2 into "conmon-v2" directory.
	@set -euo pipefail; \
	# Ensure 'conmon-v2' remote exists (add if missing)
	if git remote get-url "$(CONMON_V2_REMOTE)" >/dev/null 2>&1; then \
		echo "Remote '$(CONMON_V2_REMOTE)' exists -> $$(git remote get-url $(CONMON_V2_REMOTE))"; \
	else \
		echo "Adding remote '$(CONMON_V2_REMOTE)' -> $(CONMON_V2_URL)"; \
		git remote add "$(CONMON_V2_REMOTE)" "$(CONMON_V2_URL)"; \
	fi; \
	\
	# Ensure we know the latest remote state
	git fetch "$(CONMON_V2_REMOTE)" "$(CONMON_V2_BRANCH)"; \
	\
	# Add the worktree if it doesn't exist/registered
	if ! git worktree list --porcelain | awk '/^worktree /{print $$2}' | grep "/$(CONMON_V2_DIR)" >/dev/null 2>&1; then \
		if [ -d "$(CONMON_V2_DIR)" ]; then \
			echo "ERROR: $(CONMON_V2_DIR) exists but is not a git worktree."; \
			exit 1; \
		fi; \
		echo "Adding worktree at $(CONMON_V2_DIR) -> $(CONMON_V2_REMOTE)/$(CONMON_V2_BRANCH)"; \
		git worktree add --force "$(CONMON_V2_DIR)" "$(CONMON_V2_REMOTE)/$(CONMON_V2_BRANCH)"; \
	fi; \
	# Re-create the CONMON_V2_DIR in case it has been removed.
	if [ ! -d "$(CONMON_V2_DIR)" ]; then \
		git worktree add --force "$(CONMON_V2_DIR)" "$(CONMON_V2_REMOTE)/$(CONMON_V2_BRANCH)"; \
	fi; \
	\
	# Update the worktree to the latest origin/main
	git -C "$(CONMON_V2_DIR)" fetch "$(CONMON_V2_REMOTE)" "$(CONMON_V2_BRANCH)"; \
	git -C "$(CONMON_V2_DIR)" checkout -B "$(CONMON_V2_BRANCH)" "$(CONMON_V2_REMOTE)/$(CONMON_V2_BRANCH)"; \
	git -C "$(CONMON_V2_DIR)" reset --hard "$(CONMON_V2_REMOTE)/$(CONMON_V2_BRANCH)"; \
	git -C "$(CONMON_V2_DIR)" clean -fdx; \
	echo "Worktree ready at $(CONMON_V2_DIR) -> $$(git -C "$(CONMON_V2_DIR)" rev-parse --short HEAD)"
