.PHONY: backend tunnel

# macOS Local Network Privacy blocks unsigned binaries from reaching LAN IPs.
# Workaround: SSH tunnel through localhost (Apple-signed, bypasses restriction).
# Run `make tunnel` once, then `make backend` to build and start.

tunnel:
	@echo "Forwarding localhost:4001 → 192.168.1.105:4000 via SSH..."
	ssh -f -N -L 4001:localhost:4000 192.168.1.105

backend:
	cargo build 2>&1 | tail -3
	PORT=3000 ORD_BASE_URL=http://localhost:4001 ./target/debug/bitmap-render-backend
