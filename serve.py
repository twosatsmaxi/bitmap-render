#!/usr/bin/env python3
"""Dev server that proxies /api/* requests to a remote backend."""
import sys
import urllib.request
import urllib.error
from http.server import SimpleHTTPRequestHandler, HTTPServer

BACKEND = sys.argv[1] if len(sys.argv) > 1 else "http://r2d2.local:3020"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8080


class ProxyHandler(SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/api/"):
            self._proxy()
        else:
            super().do_GET()

    def _proxy(self):
        url = f"{BACKEND}{self.path}"
        try:
            req = urllib.request.Request(url)
            with urllib.request.urlopen(req, timeout=10) as resp:
                body = resp.read()
                self.send_response(resp.status)
                skip = {"connection", "keep-alive", "transfer-encoding"}
                for key, val in resp.headers.items():
                    if key.lower() not in skip:
                        self.send_header(key, val)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
        except urllib.error.HTTPError as e:
            self.send_error(e.code, e.reason)
        except urllib.error.URLError as e:
            self.send_error(502, str(e.reason))


if __name__ == "__main__":
    print(f"Serving on http://localhost:{PORT}")
    print(f"Proxying /api/* -> {BACKEND}")
    HTTPServer(("", PORT), ProxyHandler).serve_forever()
