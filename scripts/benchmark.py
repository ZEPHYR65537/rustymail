#!/usr/bin/env python3
"""Reproducible native Linux lab SMTP baseline; no TLS/scanner/outbound claims."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import math
import os
from pathlib import Path
import platform
import re
import smtplib
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def percentile(values, percent):
    return sorted(values)[max(0, math.ceil(len(values) * percent / 100) - 1)]


def read_limit(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default="target/release")
    parser.add_argument("--output", default="reports/local/benchmark.json")
    args = parser.parse_args()
    if platform.system() != "Linux":
        raise SystemExit("This baseline reads native Linux /proc metrics")
    binaries = Path(args.bin_dir).resolve()
    output = Path(args.output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    cpus = sorted(os.sched_getaffinity(0))[:2]
    cases = []
    for requested_size, count, concurrency in [(1024, 100, 1), (65536, 100, 1), (65536, 100, 4), (25 * 1024**2, 4, 1)]:
        with tempfile.TemporaryDirectory(prefix="rustymail-benchmark-") as temporary:
            base = Path(temporary)
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            config = (ROOT / "deploy/rustymail.lab.toml").read_text(encoding="utf-8")
            config = re.sub(r'^data_dir = .*$', lambda _: "data_dir = " + json.dumps(str(base / "mail")), config, flags=re.M)
            config = config.replace('smtp = "127.0.0.1:2525"', f'smtp = "127.0.0.1:{port}"')
            config = config.replace("disk_reserve_bytes = 2147483648", "disk_reserve_bytes = 1")
            config = config.replace("disk_reserve_percent = 10", "disk_reserve_percent = 1")
            config = config.replace("shutdown_grace_seconds = 60", "shutdown_grace_seconds = 2")
            config_path = base / "lab.toml"
            config_path.write_text(config)
            control = [str(binaries / "rustymailctl"), "--config", str(config_path)]
            subprocess.run(control + ["account", "add", "alice@example.com"], check=True, capture_output=True)
            header = b"From: sender@remote.test\r\nTo: alice@example.com\r\nSubject: bounded baseline\r\n\r\n"
            line = b"x" * 78 + b"\r\n"
            raw = header + line * ((requested_size - len(header)) // len(line))
            with (base / "server.log").open("wb") as log:
                process = subprocess.Popen(["taskset", "-c", ",".join(map(str, cpus)), str(binaries / "rustymaild"),
                                            "--config", str(config_path), "serve-lab"], stdout=subprocess.DEVNULL, stderr=log)
                try:
                    deadline = time.monotonic() + 20
                    while True:
                        if process.poll() is not None:
                            raise RuntimeError((base / "server.log").read_text())
                        try:
                            with smtplib.SMTP("127.0.0.1", port, timeout=5) as client:
                                client.ehlo()
                            break
                        except OSError:
                            if time.monotonic() > deadline:
                                raise
                            time.sleep(0.05)
                    def worker(messages):
                        latencies = []
                        with smtplib.SMTP("127.0.0.1", port, timeout=120) as client:
                            client.ehlo()
                            for _ in range(messages):
                                started = time.perf_counter()
                                assert not client.sendmail("sender@remote.test", ["alice@example.com"], raw)
                                latencies.append((time.perf_counter() - started) * 1000)
                        return latencies
                    started = time.perf_counter()
                    with ThreadPoolExecutor(max_workers=concurrency) as pool:
                        futures = [pool.submit(worker, count // concurrency + (i < count % concurrency)) for i in range(concurrency)]
                        latencies = [value for future in futures for value in future.result()]
                    elapsed = time.perf_counter() - started
                    metrics = {}
                    for entry in Path(f"/proc/{process.pid}/status").read_text().splitlines():
                        if entry.startswith(("VmRSS:", "VmHWM:")):
                            key, value, _ = entry.split()
                            metrics[key.rstrip(":") + "_kib"] = int(value)
                    cpu_fields = Path(f"/proc/{process.pid}/stat").read_text().rsplit(") ", 1)[1].split()
                    cpu_seconds = (int(cpu_fields[11]) + int(cpu_fields[12])) / os.sysconf("SC_CLK_TCK")
                    process.terminate()
                    process.wait(timeout=30)
                    assert process.returncode == 0
                finally:
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=15)
            check = subprocess.run(control + ["check-store"], check=True, capture_output=True, text=True)
            integrity = json.loads(check.stdout)
            assert integrity["healthy"] and integrity["referenced_blobs"] == count
            cases.append({"message_bytes": len(raw), "messages": count, "concurrency": concurrency,
                          "wall_seconds": elapsed, "messages_per_second": count / elapsed,
                          "mib_per_second": len(raw) * count / (1024**2 * elapsed),
                          "latency_ms": {"p50": percentile(latencies, 50), "p95": percentile(latencies, 95), "p99": percentile(latencies, 99)},
                          "daemon_memory": metrics, "daemon_cpu_seconds_including_startup": cpu_seconds,
                          "raw_latency_ms": latencies, "integrity": integrity})
            print(f"Native SMTP baseline: {len(raw)} bytes x {count}, concurrency {concurrency} complete", flush=True)
    cpu_model = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), "unknown")
    report = {"revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
              "platform": platform.platform(), "cpu_model": cpu_model, "daemon_cpu_affinity": cpus,
              "host_memory": Path("/proc/meminfo").read_text().splitlines()[0],
              "cgroup_cpu_max": read_limit("/sys/fs/cgroup/cpu.max"), "cgroup_memory_max": read_limit("/sys/fs/cgroup/memory.max"),
              "filesystem": subprocess.check_output(["findmnt", "-T", tempfile.gettempdir(), "-n", "-o", "FSTYPE,OPTIONS"], text=True).strip(),
              "profile": "release; loopback plaintext SMTP; WAL/FULL; one writer; 16 KiB buffers; 8 MiB SQLite cache",
              "latency_scope": "MAIL/RCPT/DATA through final 250; reused connections; excludes initial TCP/EHLO",
              "memory_scope": "daemon VmHWM including Rust/SQLite, excludes Python driver; host has no imposed 2 GiB RAM cap",
              "limitations": "short CI baseline on one host, no TLS/MIME scanner/outbound, no production throughput guarantee", "cases": cases}
    output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print("RUSTYMAIL_BENCHMARK_REPORT:" + json.dumps(report), flush=True)


if __name__ == "__main__":
    main()
