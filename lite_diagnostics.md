# lite.html Diagnostics

This document captures the failure modes and applied fixes when `lite.html` requests data from `/api/block/:height` while running locally. It provides context for follow-up agents to reconnect the UI to the appropriate proxy.

## Context

When `lite.html` is served (e.g. from `localhost:8000`) and the Rust backend is running on a different port or in a dev sandbox, requests to `/api/block/:height` can fail due to network, DNS, and CORS constraints.

## Applied Fixes

To allow `lite.html` to communicate with the backend under these constraints, the following changes were made:

1. **CORS Configuration (Backend)**: Added `tower-http` CORS layer to the Axum router in `src/main.rs`. This allows cross-origin requests (`Allow-Origin: *`) for `GET` methods, addressing the browser's CORS policy restrictions when the frontend and backend are on different ports or domains.
2. **Dynamic API_BASE (Frontend)**: Updated `lite.html` to determine `API_BASE` dynamically. It now checks for an `api_base` URL parameter (`?api_base=...`) and defaults to `http://localhost:3000`. This allows the UI to point to different backend URLs without modifying the HTML file.

## Remaining Failure Modes and Network Constraints

In certain dev sandbox environments, additional constraints have been observed:
- **Backend DNS/Network Isolation**: The frontend might not be able to resolve or connect to the backend if it is isolated or if specific ports are blocked.
- **Proxy Disconnection**: If the environment uses a proxy to route requests between the frontend and backend, the current default (`http://localhost:3000`) or missing `api_base` URL parameter can cause requests to fail.

## Next Steps

A follow-up agent must reconnect the UI to the appropriate proxy by:
1. Ensuring the correct proxy URL is passed to `lite.html` via the `?api_base=` query parameter.
2. Verifying that the backend's CORS policy aligns with the origin of the proxy.
3. Checking the dev sandbox network configuration to ensure the proxy can route traffic between `lite.html` and the backend service.