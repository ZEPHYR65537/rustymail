#!/usr/bin/env python3
"""Bounded short SMTP saturation/recovery experiment; Linux records process RSS/FDs.

This is not a soak test, service-level benchmark, or a substitute for a VPS trial.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import smtplib
import socket
import statistics
import time
from smtp_lab import Lab, report


def snapshot(pid):
    if os.name != 'posix' or not Path(f'/proc/{pid}/status').exists():
        return None
    result = {}
    for line in Path(f'/proc/{pid}/status').read_text().splitlines():
        if line.startswith(('VmRSS:', 'VmHWM:', 'Threads:')):
            key, value, *_ = line.split()
            result[key.rstrip(':')] = int(value)
    result['fds'] = len(list(Path(f'/proc/{pid}/fd').iterdir()))
    return result


def begin(client):
    assert client.mail('sender@remote.test')[0] == 250
    assert client.rcpt('alice@example.com')[0] == 250
    return client.docmd('DATA')[0]


def eventually(function, seconds=6):
    deadline = time.monotonic()+seconds
    while True:
        try:
            return function()
        except (AssertionError, OSError, smtplib.SMTPException):
            if time.monotonic() >= deadline:
                raise
            time.sleep(.025)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', default='target/release')
    parser.add_argument('--output', default='reports/local/smtp-pressure.json')
    args = parser.parse_args()
    checks, samples = [], {}
    with Lab(args.bin_dir, {'data_idle_seconds':10, 'data_total_seconds':20}) as lab:
        samples['idle'] = snapshot(lab.process.pid)
        # Admit eight banner-complete sessions; no scheduling guesses or sleep.
        holders = []
        try:
            for _ in range(8):
                holders.append(lab.connect())
            for role in ['smtp', 'submission', 'submissions']:
                with socket.create_connection(('127.0.0.1', lab.ports[role]), timeout=5) as rejected:
                    with rejected.makefile('rb') as reader:
                        response = reader.readline(512)
                        # Implicit TLS cannot send an unencrypted SMTP error.
                        assert response == b'' if role == 'submissions' else response.startswith(b'421 '), (role, response)
            samples['connection_saturation'] = snapshot(lab.process.pid)
        finally:
            for client in holders:
                client.close()
        def probe():
            with lab.connect() as client:
                assert client.noop()[0] == 250
        eventually(probe)
        checks.append('P01: eight held sessions saturate shared admission; all three listeners reject; disconnect restores service')

        holders = []
        try:
            for _ in range(2):
                client = lab.connect()
                holders.append(client)
                assert client.docmd('STARTTLS')[0] == 220
            with lab.connect() as client:
                assert client.docmd('STARTTLS')[0] == 454
                assert client.noop()[0] == 250
            samples['handshake_saturation'] = snapshot(lab.process.pid)
        finally:
            for client in holders:
                client.close()
        def tls_probe():
            with lab.connect('submissions') as client:
                assert client.noop()[0] == 250
        eventually(tls_probe)
        checks.append('P02: two incomplete TLS handshakes exhaust handshake permits; 454 precedes upgrade; cancellation restores TLS')

        # A DATA-stage disconnect exercises async file/permit cancellation.
        for _ in range(12):
            holders = []
            try:
                for _ in range(2):
                    client = lab.connect()
                    holders.append(client)
                    assert begin(client) == 354
                    client.sock.sendall(b'Subject: incomplete\r\n\r\npartial')
                with lab.connect() as client:
                    assert begin(client) == 452
                    assert client.docmd('DATA')[0] == 503
                samples['ingest_saturation'] = snapshot(lab.process.pid)
            finally:
                for client in holders:
                    client.close()
            def drained():
                assert not list((lab.base/'mail'/'staging').iterdir())
                probe()
            eventually(drained)
        checks.append('P03: twelve two-DATA cancellation rounds enforce ingest/temp budgets and delete every abandoned staging file')

        count = 40
        def worker(role):
            latencies, retries = [], 0
            # Concurrent workers include two real Argon2 authentications and
            # TLS transports. Reuse sessions to avoid confusing auth rate policy
            # with delivery throughput. Noise connections never authenticate.
            with lab.connect(role, authenticate=role != 'smtp') as client:
                sender = 'sender@remote.test' if role == 'smtp' else 'alice@example.com'
                for index in range(count):
                    raw = (f'From: {sender}\r\nSubject: {role}-{index}\r\n\r\n'.encode()
                           + (b'x'*78+b'\r\n')*50)
                    started, deadline = time.perf_counter(), time.monotonic()+20
                    while True:
                        try:
                            assert client.sendmail(sender, ['alice@example.com'], raw) == {}
                            break
                        except smtplib.SMTPDataError as error:
                            assert error.smtp_code == 452 and time.monotonic() < deadline
                            retries += 1
                            time.sleep(.01)
                    latencies.append((time.perf_counter()-started)*1000)
            return {'role':role, 'latencies_ms':latencies, 'capacity_retries':retries}
        def noise():
            for _ in range(40):
                client = lab.connect()
                try:
                    client.sock.sendall(b'NO\0OP\r\nMAIL FROM:<>\r\n')
                    assert client.getreply()[0] == 500
                finally:
                    client.close()
        started = time.perf_counter()
        with ThreadPoolExecutor(max_workers=4) as pool:
            futures = [pool.submit(worker, role) for role in ['smtp', 'submission', 'submissions']]
            malicious = pool.submit(noise)
            workers = [future.result(timeout=120) for future in futures]
            malicious.result(timeout=30)
        elapsed = time.perf_counter()-started
        eventually(probe)
        samples['drained'] = snapshot(lab.process.pid)
        if samples['drained'] is not None:
            # Two 64 MiB hashes plus SQLite/TLS/runtime under a deliberately
            # loose 256 MiB regression ceiling, not a 128 MiB production promise.
            assert samples['drained']['VmHWM'] <= 256*1024, samples
            assert samples['drained']['fds'] <= samples['idle']['fds']+4, samples
        integrity = lab.check_store(count*3)
        checks.append('P04: 120 durable deliveries across three roles finish amid forty malformed connections; capacity retries bounded; store healthy')
        values = sorted(value for worker in workers for value in worker['latencies_ms'])
        report(args.output, checks=checks, accepted_messages=count*3, workers=workers, samples=samples,
               limits={'connections':8, 'handshakes':2, 'ingest':2, 'temporary_bytes':135168, 'rss_hwm_ceiling_kib':262144},
               wall_seconds=elapsed, messages_per_second=count*3/elapsed,
               latency_ms={'p50':statistics.median(values), 'p95':values[int(.95*(len(values)-1))], 'max':max(values)},
               linux_resource_checks=samples['drained'] is not None, integrity=integrity)


if __name__ == '__main__':
    main()
