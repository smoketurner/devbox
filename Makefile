# Makefile for devbox

# Load .env file if present (silently skip if not found)
-include .env
export

# Tool binaries
CARGO ?= cargo
DOCKER ?= docker

# Docker image configuration
IMAGE_NAME ?= devbox
IMAGE_TAG ?= latest

.PHONY: all build test run-server clean help docker-build docker-run css-dev css-build fmt lint check bake-cli bake-server bake-agent bake-all

all: build

##@ Build

build: css-build ## Build the devbox binaries (includes CSS)
	$(CARGO) build --release

##@ Development

css-dev: ## Watch and rebuild CSS (requires tailwindcss CLI)
	cd crates/devbox-server && tailwindcss -i styles/input.css -o static/css/output.css --watch

css-build: ## Build minified CSS for production
	cd crates/devbox-server && tailwindcss -i styles/input.css -o static/css/output.css --minify

run-server: css-build ## Run the devbox server locally (loads .env if present)
	RUST_LOG=info,devbox_server=debug $(CARGO) run --bin devbox-server -- ${ARGS}

fmt: ## Format code
	$(CARGO) fmt --all

lint: ## Run clippy lints
	$(CARGO) clippy --all-targets --all-features -- -D warnings

check: ## Run cargo check
	$(CARGO) check

##@ Testing

test: ## Run unit tests
	$(CARGO) test --all-features

##@ Docker

docker-build: ## Build Docker image
	$(DOCKER) build -t $(IMAGE_NAME):$(IMAGE_TAG) .

docker-run: ## Run Docker container locally
	$(DOCKER) run --rm -it \
		-p 3000:3000 \
		-e DATABASE_URL=sqlite:/data/devbox.db?mode=rwc \
		-e RUST_LOG=debug \
		-v devbox-data:/data \
		$(IMAGE_NAME):$(IMAGE_TAG)

##@ Docker Bake (musl builds)

bake-cli: ## Build CLI musl binary via Docker Bake
	$(DOCKER) buildx bake cli

bake-server: ## Build Server musl binary via Docker Bake
	$(DOCKER) buildx bake server

bake-agent: ## Build Agent musl binary via Docker Bake
	$(DOCKER) buildx bake agent

bake-all: ## Build all musl binaries via Docker Bake
	$(DOCKER) buildx bake ci

##@ Cleanup

clean: ## Clean build artifacts
	$(CARGO) clean

docker-clean: ## Remove Docker images
	$(DOCKER) rmi $(IMAGE_NAME):$(IMAGE_TAG) || true
	$(DOCKER) volume rm devbox-data || true

##@ Help

help: ## Display this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } ' $(MAKEFILE_LIST)
