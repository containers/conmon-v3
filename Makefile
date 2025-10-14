MAKEFILE_PATH := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
BINARY := conmon
CONTAINER_RUNTIME ?= podman
BUILD_DIR ?= .build
TEST_FLAGS ?=
PACKAGE_NAME ?= $(shell cargo metadata --no-deps --format-version 1 | jq -r '.packages[2] | [ .name, .version ] | join("-v")')
PREFIX ?= /usr
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
default: ## Build the conmon binary.
	cargo build

.PHONY: release
release: ## Build the conmon binary in release mode.
	cargo build --release

.PHONY: release-static
release-static: ## Build the conmon binary in release-static mode.
	RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-unknown-linux-gnu
	strip -s target/x86_64-unknown-linux-gnu/release/conmon
	ldd target/x86_64-unknown-linux-gnu/release/conmon 2>&1 | grep -qE '(statically linked)|(not a dynamic executable)'

##@ Test targets:

.PHONY: test
test: unit e2e ## Run both `unit` and `e2e` tests

.PHONY: unit
unit: ## Run the unit tests.
	cargo test --no-fail-fast

.PHONY: e2e
e2e: conmon-v2 ## Run the e2e tests.
	CONMON_BINARY="$(MAKEFILE_PATH)target/debug/conmon" conmon-v2/test/run-tests.sh

.PHONY: lint
lint: ## Run the linter.
	cargo fmt && git diff --exit-code
	cargo clippy --all-targets --all-features -- -D warnings

##@ Utility targets:

.PHONY: prettier
prettier: ## Prettify supported files.
	$(CONTAINER_RUNTIME) run -it --privileged -v ${PWD}:/w -w /w --entrypoint bash node:latest -c \
		'npm install -g prettier && prettier -w .'

.PHONY: clean
clean: ## Cleanup the project files.
	rm -rf target/
	rm -rf conmon-v2/

.PHONY: install
install: ## Install the binary.
	mkdir -p "${DESTDIR}$(PREFIX)/bin"
	install -D -t "${DESTDIR}$(PREFIX)/bin" target/release/conmon

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
	\
	# Update the worktree to the latest origin/main
	git -C "$(CONMON_V2_DIR)" fetch "$(CONMON_V2_REMOTE)" "$(CONMON_V2_BRANCH)"; \
	git -C "$(CONMON_V2_DIR)" checkout -B "$(CONMON_V2_BRANCH)" "$(CONMON_V2_REMOTE)/$(CONMON_V2_BRANCH)"; \
	git -C "$(CONMON_V2_DIR)" reset --hard "$(CONMON_V2_REMOTE)/$(CONMON_V2_BRANCH)"; \
	git -C "$(CONMON_V2_DIR)" clean -fdx; \
	echo "Worktree ready at $(CONMON_V2_DIR) -> $$(git -C "$(CONMON_V2_DIR)" rev-parse --short HEAD)"
