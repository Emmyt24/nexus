#!/usr/bin/env python3
"""Serve the consult tester and the API from one origin.

A single origin means one tunnel URL, no CORS, no mixed content, and the
LiveKit webhook lands on the same host the browser is already using.

    ./dev/tunnel-proxy.py            # then: ngrok http 5100

Everything under /api/ and /health is proxied to the backend; everything else
is served from this directory. Development only — there is no auth here beyond
what the backend already enforces.
"""
import http.server
import os
import socketserver
import sys
import urllib.error
import urllib.request

PORT = int(os.environ.get("PROXY_PORT", "5100"))
BACKEND = os.environ.get("BACKEND", "http://127.0.0.1:8080")
PROXY_PREFIXES = ("/api/", "/health")
# Hop-by-hop headers must not be forwarded (RFC 7230 §6.1).
HOP_BY_HOP = {
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
    "te", "trailers", "transfer-encoding", "upgrade", "host", "content-length",
}


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=os.path.dirname(os.path.abspath(__file__)), **kw)

    def _is_proxied(self):
        return self.path.startswith(PROXY_PREFIXES)

    def _proxy(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None

        headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP_BY_HOP}
        req = urllib.request.Request(
            BACKEND + self.path, data=body, method=self.command, headers=headers
        )
        try:
            with urllib.request.urlopen(req, timeout=60) as res:
                payload, status, out = res.read(), res.status, res.headers
        except urllib.error.HTTPError as e:
            # A 4xx/5xx from the backend is a real answer — pass it through
            # verbatim so the client sees the actual status and body.
            payload, status, out = e.read(), e.code, e.headers
        except Exception as e:                                    # noqa: BLE001
            self.send_error(502, f"backend unreachable: {e}")
            return

        self.send_response(status)
        for k, v in out.items():
            if k.lower() not in HOP_BY_HOP:
                self.send_header(k, v)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        self._proxy() if self._is_proxied() else super().do_GET()

    def do_HEAD(self):
        self._proxy() if self._is_proxied() else super().do_HEAD()

    def do_POST(self):
        self._proxy()

    def do_PUT(self):
        self._proxy()

    def do_PATCH(self):
        self._proxy()

    def do_DELETE(self):
        self._proxy()

    def do_OPTIONS(self):
        self._proxy()

    def log_message(self, fmt, *args):
        sys.stderr.write("%s %s\n" % (self.address_string(), fmt % args))


class Server(socketserver.ThreadingTCPServer):
    # The page polls the API while holding the video connection open, so a
    # single-threaded server would deadlock on itself.
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    with Server(("0.0.0.0", PORT), Handler) as httpd:
        print(f"serving {os.path.dirname(os.path.abspath(__file__))} on :{PORT}, "
              f"proxying {PROXY_PREFIXES} -> {BACKEND}", flush=True)
        httpd.serve_forever()
