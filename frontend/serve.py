#!/usr/bin/env python3
import functools
import json
import http.server
import urllib.error
import urllib.request
from pathlib import Path

WATCH = "http://localhost:9000/lambda-url"
ROUTED = {"/api/admin": f"{WATCH}/admin/", "/api/draw": f"{WATCH}/draw-run/"}
FORWARDED = ("authorization", "content-type")


class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/api/"):
            self.forward(f"{WATCH}/api" + self.path[len("/api"):])
        else:
            super().do_GET()

    def do_POST(self):
        if self.path in ROUTED:
            self.forward(ROUTED[self.path])
        elif self.path.startswith("/api/"):
            self.forward(f"{WATCH}/api" + self.path[len("/api"):])
        else:
            self.send_error(404, "no such route")

    def forward(self, url):
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length) if length else None
        headers = {name: self.headers[name] for name in FORWARDED if self.headers.get(name)}
        request = urllib.request.Request(url, data=body, headers=headers, method=self.command)

        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                status, payload, kind = response.status, response.read(), response.headers.get("content-type", "application/json")
        except urllib.error.HTTPError as error:
            status, payload, kind = error.code, error.read(), error.headers.get("content-type", "application/json")
        except urllib.error.URLError as error:
            status, payload, kind = 502, json.dumps({"error": f"502 api unreachable: {error.reason}"}).encode(), "application/json"

        self.send_response(status)
        self.send_header("content-type", kind)
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def main():
    handler = functools.partial(Handler, directory=str(Path(__file__).parent))
    print(f"frontend on http://localhost:3000, /api -> {WATCH}/api, {' and '.join(ROUTED)} -> their own functions")
    http.server.ThreadingHTTPServer(("0.0.0.0", 3000), handler).serve_forever()


if __name__ == "__main__":
    main()
