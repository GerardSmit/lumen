"""A small OpenAI-compatible chat client with a mock server for its own integration test.

`llm_client.py --serve` runs the mock server (http.server); the default mode starts that server
as a child process, then talks to it with urllib.request: plain completions, retries on 429,
auth errors, server-sent-event streaming, image inputs as base64 data URLs and a response cache
keyed by a SHA-256 of the request.
"""

import argparse
import base64
import contextlib
import hashlib
import io
import json
import subprocess
import sys
import textwrap
import time
import urllib.error
import urllib.request
from dataclasses import asdict, dataclass, field
from http.server import BaseHTTPRequestHandler, HTTPServer
from typing import Dict, Iterator, List, Optional

API_KEY = "sk-test-0123456789"


# ---- the mock server -----------------------------------------------------------------------------

class MockLLMHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    rate_limited_once = False

    def log_message(self, format, *args):
        pass

    def _send_json(self, status: int, payload: dict, headers: Optional[Dict[str, str]] = None) -> None:
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/v1/models":
            self._send_json(200, {"object": "list", "data": [{"id": "mock-small"}, {"id": "mock-large"}]})
        else:
            self._send_json(404, {"error": {"message": f"no route for {self.path}"}})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length)
        if self.path == "/shutdown":
            self._send_json(200, {"bye": True})
            self.server.done = True
            return
        if self.headers.get("Authorization") != f"Bearer {API_KEY}":
            self._send_json(401, {"error": {"message": "invalid api key", "type": "auth"}})
            return
        req = json.loads(raw)
        if req.get("model") == "mock-busy" and not MockLLMHandler.rate_limited_once:
            MockLLMHandler.rate_limited_once = True
            self._send_json(429, {"error": {"message": "slow down"}}, {"Retry-After": "0"})
            return
        last = req["messages"][-1]
        content = last["content"]
        images = []
        if isinstance(content, list):
            texts = [p["text"] for p in content if p["type"] == "text"]
            for p in content:
                if p["type"] == "image_url":
                    header, _, data = p["image_url"]["url"].partition(",")
                    images.append(f"{header.split(';')[0][5:]}:{hashlib.sha256(base64.b64decode(data)).hexdigest()[:12]}")
            content = " ".join(texts)
        words = content.split()
        reply = " ".join(reversed(words))
        if images:
            reply += " [saw " + ", ".join(images) + "]"
        if req.get("system_check"):
            reply = req["messages"][0]["content"].upper()
        if req.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            for i, word in enumerate(reply.split()):
                chunk = {"choices": [{"index": 0, "delta": {"content": ("" if i == 0 else " ") + word}}]}
                self._chunk(f"data: {json.dumps(chunk)}\n\n".encode())
            self._chunk(b"data: [DONE]\n\n")
            self._chunk(b"")
            return
        prompt_tokens = sum(len(str(m["content"]).split()) for m in req["messages"])
        self._send_json(200, {
            "id": "chatcmpl-" + hashlib.md5(raw).hexdigest()[:8],
            "model": req["model"],
            "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": len(reply.split()),
                      "total_tokens": prompt_tokens + len(reply.split())},
        })

    def _chunk(self, data: bytes) -> None:
        self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")


def serve() -> None:
    server = HTTPServer(("127.0.0.1", 0), MockLLMHandler)
    server.done = False
    print(server.server_address[1], flush=True)
    while not server.done:
        server.handle_request()
    server.server_close()


# ---- the client ------------------------------------------------------------------------------

@dataclass
class Message:
    role: str
    content: object


@dataclass
class ChatRequest:
    model: str
    messages: List[Message]
    temperature: float = 0.7
    max_tokens: Optional[int] = None
    stream: bool = False
    extra: Dict[str, object] = field(default_factory=dict)

    def payload(self) -> dict:
        data = {k: v for k, v in asdict(self).items() if v is not None and k != "extra"}
        data.update(self.extra)
        return data

    def cache_key(self) -> str:
        canonical = json.dumps(self.payload(), sort_keys=True, separators=(",", ":"))
        return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


class APIError(Exception):
    def __init__(self, status: int, message: str):
        super().__init__(f"HTTP {status}: {message}")
        self.status = status


class Client:
    def __init__(self, base_url: str, api_key: str, max_retries: int = 2):
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.max_retries = max_retries
        self.cache: Dict[str, dict] = {}
        self.requests_sent = 0

    def _request(self, method: str, path: str, body: Optional[dict] = None):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base_url + path, data=data, method=method, headers={
            "Authorization": f"Bearer {self.api_key}",
            "Content-Type": "application/json",
            "User-Agent": "lumen-llm-client/0.1",
        })
        self.requests_sent += 1
        return urllib.request.urlopen(req, timeout=10)

    def _call(self, method: str, path: str, body: Optional[dict] = None) -> dict:
        for attempt in range(self.max_retries + 1):
            try:
                with self._request(method, path, body) as resp:
                    return json.load(resp)
            except urllib.error.HTTPError as e:
                detail = json.loads(e.read() or b"{}").get("error", {}).get("message", e.reason)
                if e.code == 429 and attempt < self.max_retries:
                    wait = float(e.headers.get("Retry-After", "1"))
                    print(f"  rate limited (attempt {attempt + 1}), retrying in {wait:g}s")
                    time.sleep(wait)
                    continue
                raise APIError(e.code, detail) from None
        raise AssertionError("unreachable")

    def models(self) -> List[str]:
        return [m["id"] for m in self._call("GET", "/v1/models")["data"]]

    def chat(self, request: ChatRequest) -> dict:
        key = request.cache_key()
        if key in self.cache:
            return {**self.cache[key], "cached": True}
        result = self._call("POST", "/v1/chat/completions", request.payload())
        self.cache[key] = result
        return result

    def stream(self, request: ChatRequest) -> Iterator[str]:
        request.stream = True
        with self._request("POST", "/v1/chat/completions", request.payload()) as resp:
            print("  content-type:", resp.headers.get_content_type())
            for raw in resp:
                line = raw.decode("utf-8").rstrip("\n")
                if not line.startswith("data: "):
                    continue
                data = line[len("data: "):]
                if data == "[DONE]":
                    return
                yield json.loads(data)["choices"][0]["delta"]["content"]


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="llm", description="Chat with an OpenAI-compatible endpoint.",
                                     formatter_class=lambda prog: argparse.HelpFormatter(prog, width=80))
    parser.add_argument("prompt", nargs="*", help="the user message")
    parser.add_argument("-m", "--model", default="mock-small", choices=["mock-small", "mock-large", "mock-busy"])
    parser.add_argument("-t", "--temperature", type=float, default=0.7)
    parser.add_argument("--max-tokens", type=int)
    parser.add_argument("--system", help="system prompt")
    parser.add_argument("--image", action="append", default=[], metavar="FILE")
    parser.add_argument("--stream", action="store_true")
    parser.add_argument("--serve", action="store_true", help=argparse.SUPPRESS)
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    if args.serve:
        serve()
        return 0

    print(textwrap.indent(parser.format_help(), "| "), end="")
    for argv in (["hello", "there", "-t", "0.2", "--max-tokens", "64"], ["--model", "gpt-9", "hi"], ["-t", "warm"]):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            try:
                ns = parser.parse_args(argv)
                print("parsed:", sorted((k, v) for k, v in vars(ns).items()))
            except SystemExit as e:
                print("exit", e.code, "->", err.getvalue().strip().splitlines()[-1])

    server = subprocess.Popen([sys.executable, __file__, "--serve"], stdout=subprocess.PIPE, text=True)
    try:
        port = int(server.stdout.readline())
        client = Client(f"http://127.0.0.1:{port}/", API_KEY)
        print("models:", client.models())

        req = ChatRequest("mock-small", [Message("user", "the quick brown fox jumps over the lazy dog")], temperature=0.0)
        print("cache key:", req.cache_key()[:16])
        res = client.chat(req)
        print(res["id"].startswith("chatcmpl-"), res["choices"][0]["message"]["content"])
        print("usage:", res["usage"])
        print("cached:", client.chat(req).get("cached", False), "requests:", client.requests_sent)

        busy = ChatRequest("mock-busy", [Message("user", "are you there")], max_tokens=5)
        print("busy:", client.chat(busy)["choices"][0]["message"]["content"])

        png = bytes([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) + bytes(range(64))
        url = "data:image/png;base64," + base64.b64encode(png).decode("ascii")
        print("data url:", url[:40] + "...", len(url))
        vision = ChatRequest("mock-large", [Message("user", [
            {"type": "text", "text": "what is in this picture"},
            {"type": "image_url", "image_url": {"url": url}},
        ])])
        print("vision:", client.chat(vision)["choices"][0]["message"]["content"])

        sys_req = ChatRequest("mock-small", [Message("system", "you are terse"), Message("user", "hi")],
                              extra={"system_check": True})
        print("system:", client.chat(sys_req)["choices"][0]["message"]["content"])

        long_prompt = ("Lumen runs JavaScript and Python on one runtime; this sentence is long enough "
                       "to need wrapping when it comes back reversed from the mock model")
        text = "".join(client.stream(ChatRequest("mock-large", [Message("user", long_prompt)])))
        print(textwrap.fill(text, width=48, initial_indent="> ", subsequent_indent="> "))
        print("shortened:", textwrap.shorten(text, width=40, placeholder=" [...]"))

        try:
            Client(f"http://127.0.0.1:{port}", "sk-wrong").models()
            Client(f"http://127.0.0.1:{port}", "sk-wrong").chat(req)
        except APIError as e:
            print("APIError:", e, e.status)
        try:
            client._call("GET", "/v1/nothing")
        except APIError as e:
            print("APIError:", e)
        print("token:", base64.urlsafe_b64encode(hashlib.sha256(API_KEY.encode()).digest()).rstrip(b"=").decode())
        print("blake2b:", hashlib.blake2b(b"lumen", digest_size=16).hexdigest(), hashlib.sha1(b"").hexdigest())

        with urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{port}/shutdown", data=b"{}")) as r:
            print("shutdown:", r.status, json.load(r))
    finally:
        print("server exit code:", server.wait(timeout=10))
        server.stdout.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
