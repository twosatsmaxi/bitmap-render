.PHONY: backend dev frontend-build frontend-watch tunnel tunnel-db all clean

DATABASE_URL ?= postgres://bitmap:nojbuw-rUrqyc-gygqi7@127.0.0.1:5433/bitmap
ORD_BASE_URL ?= http://localhost:4001

# macOS Local Network Privacy blocks unsigned binaries from reaching LAN IPs.
# Workaround: SSH tunnel through localhost (Apple-signed, bypasses restriction).
# Run `make tunnel` once, then `make backend` to build and start.

tunnel:
	@echo "Forwarding localhost:4001 → 192.168.1.105:4000 via SSH..."
	ssh -f -N -L 4001:localhost:4000 umbrel@192.168.1.105

tunnel-db:
	@echo "Forwarding localhost:5433 → 192.168.1.105:5432 via SSH..."
	ssh -f -N -L 5433:localhost:5432 umbrel@192.168.1.105

frontend-build:
	cd frontend && trunk build

frontend-watch:
	cd frontend && trunk watch

backend:
	DATABASE_URL=$(DATABASE_URL) ORD_BASE_URL=$(ORD_BASE_URL) cargo run -p bitmap-render-backend --release

# Development mode: Build frontend once then run backend
dev: frontend-build
	DATABASE_URL=$(DATABASE_URL) PORT=3000 ORD_BASE_URL=$(ORD_BASE_URL) cargo run -p bitmap-render-backend

all: frontend-build
	cargo build --release

clean:
	cargo clean
	rm -rf frontend/dist
