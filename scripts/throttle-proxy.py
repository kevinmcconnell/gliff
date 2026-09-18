#!/usr/bin/env python3
"""A TCP proxy that limits the server->client direction to a fixed bit rate
and adds a fixed one-way delay, to stand in for a constrained link when
`tc netem` is not available.

    throttle-proxy.py LISTEN_PORT TARGET_PORT MBIT [DELAY_MS] [QUEUE_KB]

Bytes above the rate wait in a bounded queue (like a router buffer); when the
queue is full the sender's write blocks, which is what TCP does on a real
saturated link.
"""
import asyncio
import sys
import time


async def shape(reader, writer, mbit, delay_ms, queue_bytes):
    rate = mbit * 1e6 / 8
    tokens = 0.0
    last = time.monotonic()
    chunk = 4096
    delay = delay_ms / 1000.0
    pending = []
    try:
        while True:
            data = await reader.read(chunk)
            if not data:
                break
            now = time.monotonic()
            tokens = min(queue_bytes, tokens + (now - last) * rate)
            last = now
            if tokens < len(data):
                wait = (len(data) - tokens) / rate
                await asyncio.sleep(wait)
                tokens = 0.0
                last = time.monotonic()
            else:
                tokens -= len(data)
            if delay > 0:
                pending.append((time.monotonic() + delay, data))
                while pending and pending[0][0] <= time.monotonic():
                    writer.write(pending.pop(0)[1])
                if pending:
                    due, d = pending[0]
                    await asyncio.sleep(max(0.0, due - time.monotonic()))
                    writer.write(d)
                    pending.pop(0)
            else:
                writer.write(data)
            await writer.drain()
    finally:
        writer.close()


async def pipe(reader, writer, delay_ms):
    delay = delay_ms / 1000.0
    try:
        while True:
            data = await reader.read(65536)
            if not data:
                break
            if delay > 0:
                await asyncio.sleep(delay)
            writer.write(data)
            await writer.drain()
    finally:
        writer.close()


async def main():
    listen, target, mbit = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
    delay_ms = float(sys.argv[4]) if len(sys.argv) > 4 else 0.0
    queue_kb = float(sys.argv[5]) if len(sys.argv) > 5 else 64.0

    async def handle(creader, cwriter):
        sreader, swriter = await asyncio.open_connection("127.0.0.1", target)
        await asyncio.gather(
            shape(sreader, cwriter, mbit, delay_ms, queue_kb * 1024),
            pipe(creader, swriter, delay_ms),
            return_exceptions=True,
        )

    server = await asyncio.start_server(handle, "127.0.0.1", listen)
    async with server:
        await server.serve_forever()


asyncio.run(main())
