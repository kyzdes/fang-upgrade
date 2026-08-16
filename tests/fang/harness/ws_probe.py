#!/usr/bin/env python3
"""ws_probe — one message over /api/agents/{id}/ws, and what came back.

Why raw sockets: this box has neither `websockets` nor `websocket-client`, and
a repro that SKIPs for want of a pip install proves nothing. RFC 6455 needs
about eighty lines for one client that sends one text frame and reads frames
until the server says it is done, so those eighty lines are here.

It makes no judgement. It prints a JSON object and lets the caller grade:

  {"terminal": "...",             how the exchange ended, one word, always set:
                                  response | silent_complete | error | closed |
                                  upgrade_refused | exception
   "deltas": ["...", ...],        every {"type":"text_delta"} content, in order
   "response": {...} | null,      the {"type":"response"} frame, verbatim
   "detail": "..." | null,        the server's error text, or the exception,
                                  when there is one. A server state is reported
                                  as that state; only a failure of this probe or
                                  of the transport is reported as one.
   "frames": N}                   how many JSON frames arrived in total

Usage: ws_probe.py BASE_URL AGENT_ID MESSAGE [TOKEN] [TIMEOUT_SECS]
"""

import base64
import json
import os
import socket
import struct
import sys
import time
from urllib.parse import urlparse


def _mask(payload: bytes) -> bytes:
    key = os.urandom(4)
    return key + bytes(b ^ key[i % 4] for i, b in enumerate(payload))


def _send_text(sock, text: str) -> None:
    payload = text.encode()
    header = bytearray([0x81])  # FIN + opcode 1 (text)
    n = len(payload)
    if n < 126:
        header.append(0x80 | n)
    elif n < (1 << 16):
        header.append(0x80 | 126)
        header += struct.pack("!H", n)
    else:
        header.append(0x80 | 127)
        header += struct.pack("!Q", n)
    sock.sendall(bytes(header) + _mask(payload))


def _send_pong(sock, payload: bytes) -> None:
    header = bytearray([0x8A, 0x80 | len(payload)])
    sock.sendall(bytes(header) + _mask(payload))


class Reader:
    """Buffered reader over a socket, with a wall-clock deadline."""

    def __init__(self, sock, deadline):
        self.sock = sock
        self.buf = b""
        self.deadline = deadline

    def read(self, n: int) -> bytes:
        while len(self.buf) < n:
            remaining = self.deadline - time.time()
            if remaining <= 0:
                raise TimeoutError("deadline reached while reading")
            self.sock.settimeout(min(remaining, 30))
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("server closed the connection")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def read_line(self) -> bytes:
        while b"\r\n" not in self.buf:
            remaining = self.deadline - time.time()
            if remaining <= 0:
                raise TimeoutError("deadline reached during the handshake")
            self.sock.settimeout(min(remaining, 30))
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("server closed during the handshake")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line


def _read_frame(reader, sock):
    """One WebSocket message. Returns (opcode, payload) or None on close."""
    data = b""
    opcode = None
    while True:
        b0, b1 = reader.read(2)
        fin = b0 & 0x80
        op = b0 & 0x0F
        length = b1 & 0x7F
        if length == 126:
            length = struct.unpack("!H", reader.read(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", reader.read(8))[0]
        if b1 & 0x80:  # a server frame must not be masked; read it anyway
            reader.read(4)
        payload = reader.read(length) if length else b""
        if op == 0x8:
            return None
        if op == 0x9:
            _send_pong(sock, payload)
            continue
        if op == 0xA:
            continue
        if op != 0x0:
            opcode = op
        data += payload
        if fin:
            return (opcode, data)


def main() -> int:
    if len(sys.argv) < 4:
        print(__doc__, file=sys.stderr)
        return 2
    base_url, agent_id, message = sys.argv[1], sys.argv[2], sys.argv[3]
    token = sys.argv[4] if len(sys.argv) > 4 else ""
    timeout = float(sys.argv[5]) if len(sys.argv) > 5 else 600.0

    u = urlparse(base_url)
    host = u.hostname or "127.0.0.1"
    port = u.port or (443 if u.scheme == "https" else 80)
    path = f"/api/agents/{agent_id}/ws"

    out = {"terminal": None, "deltas": [], "response": None,
           "detail": None, "frames": 0}
    deadline = time.time() + timeout
    sock = socket.create_connection((host, port), timeout=30)
    try:
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
        )
        if token:
            req += f"Authorization: Bearer {token}\r\n"
        req += "\r\n"
        sock.sendall(req.encode())

        reader = Reader(sock, deadline)
        status = reader.read_line().decode(errors="replace")
        while reader.read_line():  # drain headers up to the blank line
            pass
        if "101" not in status:
            out["terminal"] = "upgrade_refused"
            out["detail"] = status.strip()
            print(json.dumps(out))
            return 0

        _send_text(sock, json.dumps({"type": "message", "content": message}))

        while True:
            frame = _read_frame(reader, sock)
            if frame is None:
                out["terminal"] = out["terminal"] or "closed"
                break
            _, payload = frame
            out["frames"] += 1
            try:
                ev = json.loads(payload.decode())
            except Exception:
                continue
            kind = ev.get("type")
            if kind == "text_delta":
                out["deltas"].append(ev.get("content", ""))
            elif kind == "response":
                out["response"] = ev
                out["terminal"] = "response"
                break
            elif kind == "error":
                out["terminal"] = "error"
                out["detail"] = ev.get("content", "unspecified error")
                break
            elif kind == "silent_complete":
                # Not an error and not this probe's failure: the server said the
                # turn produced nothing to say. Reported as itself.
                out["terminal"] = "silent_complete"
                break
    except Exception as e:  # timeout, transport, anything: say so, do not judge
        out["terminal"] = "exception"
        out["detail"] = f"{type(e).__name__}: {e}"
    finally:
        try:
            sock.close()
        except Exception:
            pass
    print(json.dumps(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
