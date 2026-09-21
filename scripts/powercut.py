#!/usr/bin/env python3
"""Linux-only QEMU/ext4 fault lab. Creates and kills only disposable guests.

Requires qemu-system-x86_64, a /boot kernel with matching /lib/modules,
busybox-static, kmod, zstd, mkfs.ext4 and musl release binaries. No host mount,
physical block device, network interface, or existing mail store is used.
"""
import argparse
from collections import deque
import gzip
import hashlib
import json
import lzma
import os
from pathlib import Path
import queue
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
POINTS = ["staged", "file_synced", "renamed", "directories_synced", "before_commit",
          "after_commit", "migration_applied", "migration_committed", "gc_planned",
          "gc_unlinked", "acknowledged"]


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def newc(entries):
    """Build a minimal initramfs, including /dev/console without host mknod."""
    output = bytearray()
    for number, (name, mode, data, major, minor) in enumerate(entries + [("TRAILER!!!", 0, b"", 0, 0)], 1):
        encoded = name.encode() + b"\0"
        fields = [number, mode, 0, 0, 1, 0, len(data), 0, 0, major, minor, len(encoded), 0]
        output.extend(b"070701" + "".join(f"{value:08x}" for value in fields).encode())
        output.extend(encoded)
        output.extend(b"\0" * (-len(output) % 4))
        output.extend(data)
        output.extend(b"\0" * (-len(output) % 4))
    return gzip.compress(output, mtime=0)


def make_initrd(binary_directory, kernel, destination):
    version = kernel.name.removeprefix("vmlinuz-")
    entries = [(name, stat.S_IFDIR | 0o755, b"", 0, 0)
               for name in ["bin", "dev", "proc", "sys", "data", "modules", "tmp"]]
    entries.append(("dev/console", stat.S_IFCHR | 0o600, b"", 5, 1))
    entries.append(("bin/busybox", stat.S_IFREG | 0o755, Path(shutil.which("busybox")).read_bytes(), 0, 0))
    for applet in ["sh", "mount", "mkdir", "insmod", "cat", "sleep", "poweroff", "kill", "ip"]:
        entries.append((f"bin/{applet}", stat.S_IFLNK | 0o777, b"busybox", 0, 0))
    modules = []
    for driver in ["virtio_pci", "virtio_blk", "virtio_rng", "ext4"]:
        for line in command("modprobe", "--show-depends", "--set-version", version, driver).splitlines():
            if not line.startswith("insmod "):
                continue
            path = Path(line.split()[1])
            if path in modules:
                continue
            modules.append(path)
            data = subprocess.check_output(["zstd", "-q", "-d", "-c", str(path)]) if path.suffix == ".zst" else path.read_bytes()
            if path.suffix == ".xz":
                data = lzma.decompress(data)
            elif path.suffix == ".gz":
                data = gzip.decompress(data)
            name = path.name.removesuffix(".zst").removesuffix(".xz").removesuffix(".gz")
            entries.append((f"modules/{name}", stat.S_IFREG | 0o644, data, 0, 0))
    for name, path in [("probe", binary_directory / "examples/m1_probe"), ("rustymaild", binary_directory / "rustymaild")]:
        entries.append((name, stat.S_IFREG | 0o755, path.read_bytes(), 0, 0))
    config = (ROOT / "deploy/rustymail.lab.toml").read_text(encoding="utf-8")
    config = config.replace('data_dir = "data/lab"', 'data_dir = "/data/mail"')
    config = config.replace("disk_reserve_bytes = 2147483648", "disk_reserve_bytes = 1")
    config = config.replace("disk_reserve_percent = 10", "disk_reserve_percent = 1")
    config = config.replace("shutdown_grace_seconds = 60", "shutdown_grace_seconds = 1")
    entries.append(("lab.toml", stat.S_IFREG | 0o600, config.encode(), 0, 0))
    loads = "\n".join(f"insmod /modules/{path.name.removesuffix('.zst').removesuffix('.xz').removesuffix('.gz')}" for path in modules)
    init = """#!/bin/sh
set -eu
export PATH=/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
ip link set lo up
MODE=verify
POINT=unknown
for arg in $(cat /proc/cmdline); do
  case "$arg" in mode=*) MODE=${arg#mode=};; point=*) POINT=${arg#point=};; esac
done
__LOADS__
mount -t ext4 /dev/vda /data
if [ "$MODE" = crash ]; then
  /probe crash /data/mail "$POINT"
elif [ "$MODE" = smtp ]; then
  /probe seed /data/mail
  /rustymaild --config /lab.toml serve-lab &
  SERVER=$!
  /probe smtp-ack /data/mail
elif [ "$MODE" = smtp-full ]; then
  /probe seed /data/mail
  /rustymaild --config /lab.toml serve-lab &
  SERVER=$!
  /probe smtp-full /data/mail
  kill -TERM "$SERVER"
  wait "$SERVER"
  /probe verify /data/mail smtp-full
elif [ "$MODE" = sqlite-full ]; then
  /probe seed /data/mail
  /probe sqlite-full /data/mail
  /probe verify /data/mail sqlite-full
else
  /probe verify /data/mail "$POINT"
fi
poweroff -f
""".replace("__LOADS__", loads)
    entries.append(("init", stat.S_IFREG | 0o755, init.encode(), 0, 0))
    destination.write_bytes(newc(entries))
    return version


def boot(kernel, initrd, disk, mode, point, marker, log_file):
    args = ["qemu-system-x86_64", "-machine", "accel=tcg", "-m", "512", "-smp", "2",
            "-nographic", "-monitor", "none", "-no-reboot", "-net", "none",
            "-object", "rng-random,id=rng0,filename=/dev/urandom", "-device", "virtio-rng-pci,rng=rng0",
            "-kernel", str(kernel), "-initrd", str(initrd),
            "-append", f"console=ttyS0 rdinit=/init panic=-1 rustymail_disposable_vm=1 mode={mode} point={point}",
            "-drive", f"file={disk},format=raw,if=virtio,cache=writeback"]
    process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
    messages = queue.Queue()
    tail = deque(maxlen=80)
    observed = []
    def read():
        with log_file.open("wb") as log:
            for line in iter(process.stdout.readline, b""):
                log.write(line)
                messages.put(line.decode("utf-8", errors="replace").strip())
        messages.put(None)
    thread = threading.Thread(target=read, daemon=True)
    thread.start()
    try:
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            try:
                line = messages.get(timeout=min(1, max(0.01, deadline - time.monotonic())))
            except queue.Empty:
                continue
            if line is None:
                break
            tail.append(line)
            if line.startswith("RUSTYMAIL_"):
                observed.append(line)
            if line.startswith(marker):
                # SIGKILL is sent to QEMU itself: the guest OS cannot flush or
                # run destructors. The host and its disk remain powered on.
                process.kill()
                process.wait(timeout=15)
                thread.join(timeout=5)
                return line, observed
        if log_file.exists():
            shutil.copyfile(log_file, log_file.parent.parent / ("failed-" + log_file.name))
        raise RuntimeError(f"Guest did not reach {marker}\n" + "\n".join(tail))
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=15)
        thread.join(timeout=5)
        process.stdout.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default="target/x86_64-unknown-linux-musl/release")
    parser.add_argument("--kernel-dir", default="/boot", help="readable vmlinuz-* images with matching installed modules")
    parser.add_argument("--output", default="reports/local/powercut.json")
    args = parser.parse_args()
    if sys.platform != "linux":
        raise SystemExit("This isolated VM laboratory requires Linux")
    binary_directory = Path(args.bin_dir).resolve()
    output = Path(args.output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    kernels = [p for p in Path(args.kernel_dir).glob("vmlinuz-*") if (Path("/lib/modules") / p.name.removeprefix("vmlinuz-") / "modules.dep").is_file()]
    if not kernels:
        raise SystemExit("Install a Linux kernel with matching modules first")
    kernel = max(kernels, key=lambda p: p.stat().st_mtime)
    results = []
    with tempfile.TemporaryDirectory(prefix="rustymail-vm-", dir=output.parent) as temporary:
        base = Path(temporary).resolve()
        initrd = base / "initrd.gz"
        version = make_initrd(binary_directory, kernel, initrd)
        for point in POINTS + ["smtp-full", "sqlite-full"]:
            disk = base / f"{point}.img"
            assert disk.parent == base and not disk.exists()
            with disk.open("xb") as file:
                file.truncate(128 * 1024 * 1024)
            subprocess.run(["mkfs.ext4", "-q", "-F", "-m", "0", str(disk)], check=True)
            mode = "smtp" if point == "acknowledged" else "crash"
            if point in ["smtp-full", "sqlite-full"]:
                line, observed = boot(kernel, initrd, disk, point, point, "RUSTYMAIL_VERIFIED:", base / f"{point}.log")
                assert any(item.startswith("RUSTYMAIL_ENOSPC:") for item in observed)
            else:
                boot(kernel, initrd, disk, mode, point, f"RUSTYMAIL_POINT:{point}", base / f"{point}-cut.log")
                line, observed = boot(kernel, initrd, disk, "verify", point, "RUSTYMAIL_VERIFIED:", base / f"{point}-reopen.log")
            result = json.loads(line.split(":", 1)[1])
            result["observations"] = observed
            results.append(result)
            print(f"Linux VM: {point} passed", flush=True)
        report = {"revision": command("git", "rev-parse", "HEAD"), "kernel": version,
                  "kernel_sha256": hashlib.sha256(kernel.read_bytes()).hexdigest(),
                  "qemu": command("qemu-system-x86_64", "--version").splitlines()[0],
                  "filesystem": "ext4, 128 MiB dedicated raw image, reserved blocks 0%",
                  "guest_memory_mib": 512, "guest_cpus": 2, "accelerator": "TCG",
                  "disk_cache": "writeback, flushes honored",
                  "fault_model": "SIGKILL QEMU; guest memory lost, host OS/storage stay alive; no physical power-loss claim",
                  "results": results}
        output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print("RUSTYMAIL_POWERCUT_REPORT:" + json.dumps(report), flush=True)


if __name__ == "__main__":
    main()
