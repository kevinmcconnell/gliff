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


async def delayed_writer(queue, writer):
    try:
        while True:
            due, data = await queue.get()
            if data is None:
                break
            now = time.monotonic()
            if due > now:
                await asyncio.sleep(due - now)
            writer.write(data)
            await writer.drain()
    finally:
        writer.close()


async def shape(reader, writer, mbit, delay_ms, queue_bytes):
    """Token-bucket shaping of one direction, then a fixed delay. The bucket
    depth is the router queue: bytes beyond it wait, which stalls the sender."""
    rate = mbit * 1e6 / 8
    delay = delay_ms / 1000.0
    queue = asyncio.Queue()
    consumer = asyncio.create_task(delayed_writer(queue, writer))
    next_free = time.monotonic()
    try:
        while True:
            data = await reader.read(16384)
            if not data:
                break
            now = time.monotonic()
            next_free = max(next_free, now)
            backlog = (next_free - now) * rate
            if backlog > queue_bytes:
                await asyncio.sleep((backlog - queue_bytes) / rate)
            next_free += len(data) / rate
            await queue.put((next_free + delay, data))
    finally:
        await queue.put((0, None))
        await consumer


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
