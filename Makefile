.PHONY: backend frontend-build frontend-watch tunnel all clean

# macOS Local Network Privacy blocks unsigned binaries from reaching LAN IPs.
# Workaround: SSH tunnel through localhost (Apple-signed, bypasses restriction).
# Run `make tunnel` once, then `make backend` to build and start.

tunnel:
	@echo "Forwarding localhost:4001 → 192.168.1.105:4000 via SSH..."
	ssh -f -N -L 4001:localhost:4000 192.168.1.105

frontend-build:
	cd frontend && trunk build

frontend-watch:
	cd frontend && trunk watch

backend:
	cargo run -p bitmap-render-backend --release

# Development mode: Build frontend once then run backend
dev: frontend-build
	PORT=3000 ORD_BASE_URL=http://localhost:4001 cargo run -p bitmap-render-backend

all: frontend-build
	cargo build --release

clean:
	cargo clean
	rm -rf frontend/dist
