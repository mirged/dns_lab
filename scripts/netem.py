#!/usr/bin/env python3
"""UDP/TCP network emulator for dns_lab benchmarks.

Sits between a client and a relay and applies one-way delay, jitter, packet loss
and a bandwidth cap **in both directions**, on the real packets. That is what
`scripts/bench.sh` needs to say anything honest about latency: sleeping on the
relay's reply path measures one leg only and serialises the whole transfer.

    scripts/netem.py --listen 127.0.0.1:15500 --upstream 127.0.0.1:15501 \
        --delay-ms 70 --jitter-ms 25 --loss 0.02 --rate-kbps 800

Stats are written as one JSON line to stdout on SIGUSR1 and on exit, so the
harness can take a delta around each measured run:

    {"udp_up": 412, "udp_down": 398, "udp_dropped_up": 4, "udp_dropped_down": 10,
     "tcp_conns": 1, "tcp_bytes": 1234, "delay_sum_ms": 30000.0, "delay_n": 810}

No root required (unlike `tc netem`), and it is UDP-aware, so datagrams can be
dropped and reordered independently of the TCP fallback path.
"""

import argparse
import asyncio
import json
import random
import signal
import sys
import time

STATS = {
    "udp_up": 0,            # datagrams client -> relay accepted
    "udp_down": 0,          # datagrams relay -> client accepted
    "udp_dropped_up": 0,    # lost on the way to the relay
    "udp_dropped_down": 0,  # lost on the way back
    "tcp_conns": 0,
    "tcp_bytes": 0,
    "delay_sum_ms": 0.0,
    "delay_n": 0,
}


class Link:
    """Delay, jitter, loss and a token bucket, shared by both directions."""

    def __init__(self, delay_ms, jitter_ms, loss, rate_kbps, seed):
        self.delay_ms = delay_ms
        self.jitter_ms = jitter_ms
        self.loss = loss
        self.rate_bps = rate_kbps * 1000 / 8 if rate_kbps else 0
        self.tokens = self.rate_bps  # one second of credit to start
        self.updated = time.monotonic()
        self.rng = random.Random(seed)

    def one_way(self):
        """Sample the one-way delay for this packet, in seconds."""
        d = self.delay_ms / 1000.0
        if self.jitter_ms:
            # Uniform jitter is enough to reorder under load, which is the
            # effect that matters for a retry protocol.
            d += self.rng.uniform(-self.jitter_ms, self.jitter_ms) / 1000.0
        d = max(d, 0.0)
        STATS["delay_sum_ms"] += d * 1000.0
        STATS["delay_n"] += 1
        return d

    def drops(self):
        return self.loss > 0 and self.rng.random() < self.loss

    def spend(self, nbytes):
        """Charge bytes to the bucket; returns the extra seconds to wait."""
        if not self.rate_bps:
            return 0.0
        now = time.monotonic()
        self.tokens = min(
            self.rate_bps, self.tokens + (now - self.updated) * self.rate_bps
        )
        self.updated = now
        self.tokens -= nbytes
        if self.tokens >= 0:
            return 0.0
        return -self.tokens / self.rate_bps


class Upstream(asyncio.DatagramProtocol):
    """One connected socket per client, so replies can be routed back."""

    def __init__(self, proxy, client_addr):
        self.proxy = proxy
        self.client_addr = client_addr

    def connection_made(self, transport):
        self.transport = transport
        self.proxy.attach(self.client_addr, transport)

    def datagram_received(self, data, addr):
        self.proxy.deliver(data, self.client_addr)

    def error_received(self, exc):
        pass


class UdpProxy(asyncio.DatagramProtocol):
    def __init__(self, loop, upstream, link):
        self.loop = loop
        self.upstream = upstream
        self.link = link
        self.front = None
        self.transports = {}      # client addr -> upstream transport
        self.pending = {}         # client addr -> datagrams waiting for it

    def connection_made(self, transport):
        self.front = transport

    def attach(self, addr, transport):
        self.transports[addr] = transport
        # Anything that arrived while the upstream socket was being created is
        # forwarded now rather than dropped.
        for data in self.pending.pop(addr, []):
            self.forward(addr, data)

    def forward(self, addr, data):
        if self.link.drops():
            STATS["udp_dropped_up"] += 1
            return
        wait = self.link.one_way() + self.link.spend(len(data))
        STATS["udp_up"] += 1
        transport = self.transports[addr]
        self.loop.call_later(wait, lambda: transport.sendto(data))

    def datagram_received(self, data, addr):
        if addr not in self.transports:
            if addr in self.pending:
                self.pending[addr].append(data)
                return
            self.pending[addr] = [data]
            fut = self.loop.create_datagram_endpoint(
                lambda: Upstream(self, addr), remote_addr=self.upstream
            )
            asyncio.ensure_future(fut)
            return
        self.forward(addr, data)

    def deliver(self, data, addr):
        if self.link.drops():
            STATS["udp_dropped_down"] += 1
            return
        wait = self.link.one_way() + self.link.spend(len(data))
        STATS["udp_down"] += 1
        front = self.front
        if front is not None:
            self.loop.call_later(wait, lambda: front.sendto(data, addr))


async def tcp_client(reader, writer, upstream, link):
    try:
        ur, uw = await asyncio.open_connection(*upstream)
    except OSError:
        writer.close()
        return
    STATS["tcp_conns"] += 1

    async def pump(src, dst):
        while True:
            data = await src.read(4096)
            if not data:
                break
            STATS["tcp_bytes"] += len(data)
            await asyncio.sleep(link.one_way() + link.spend(len(data)))
            dst.write(data)
            await dst.drain()
        try:
            dst.close()
        except Exception:
            pass

    await asyncio.gather(pump(reader, uw), pump(ur, writer))
    writer.close()


async def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--listen", default="127.0.0.1:15500")
    ap.add_argument("--upstream", default="127.0.0.1:15501")
    ap.add_argument("--delay-ms", type=float, default=0)
    ap.add_argument("--jitter-ms", type=float, default=0)
    ap.add_argument("--loss", type=float, default=0.0, help="0..1, per direction")
    ap.add_argument("--rate-kbps", type=float, default=0, help="0 = unlimited")
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    def hostport(s):
        host, _, port = s.rpartition(":")
        return host, int(port)

    listen = hostport(args.listen)
    upstream = hostport(args.upstream)
    link = Link(args.delay_ms, args.jitter_ms, args.loss, args.rate_kbps, args.seed)
    loop = asyncio.get_event_loop()

    await loop.create_datagram_endpoint(
        lambda: UdpProxy(loop, upstream, link), local_addr=listen
    )
    await asyncio.start_server(lambda r, w: tcp_client(r, w, upstream, link), *listen)

    def dump(*_):
        sys.stdout.write(json.dumps(STATS) + "\n")
        sys.stdout.flush()

    def stop(*_):
        dump()
        raise SystemExit(0)

    loop.add_signal_handler(signal.SIGUSR1, dump)
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop)

    print(
        f"[netem] {listen[0]}:{listen[1]} -> {upstream[0]}:{upstream[1]} "
        f"delay={args.delay_ms}ms jitter={args.jitter_ms}ms "
        f"loss={args.loss:.1%} rate={args.rate_kbps or 'unlimited'}kbps",
        file=sys.stderr,
        flush=True,
    )
    await asyncio.Event().wait()


if __name__ == "__main__":
    asyncio.run(main())
