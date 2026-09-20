#!/usr/bin/env python3
"""Independent stdlib SMTP/CLI check, including killing after final SMTP 250.

Uses only loopback, temporary data, example addresses and child processes.
No existing server, account, certificate, or user mail configuration is touched.
"""
import argparse
from email.message import EmailMessage
from email.policy import SMTP
import json
from pathlib import Path
import re
import smtplib
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default="target/debug")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    binaries = Path(args.bin_dir).resolve()
    suffix = ".exe" if __import__("os").name == "nt" else ""
    daemon = binaries / f"rustymaild{suffix}"
    control = binaries / f"rustymailctl{suffix}"
    assert daemon.is_file() and control.is_file(), "Run cargo build --workspace --locked first"
    with tempfile.TemporaryDirectory(prefix="rustymail-smoke-") as temporary:
        base = Path(temporary).resolve()
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        config = (root / "deploy/rustymail.lab.toml").read_text(encoding="utf-8")
        config = re.sub(r'^data_dir = .*$', lambda _: "data_dir = " + json.dumps(str(base / "mail")), config, flags=re.M)
        config = config.replace('smtp = "127.0.0.1:2525"', f'smtp = "127.0.0.1:{port}"')
        config = config.replace("disk_reserve_bytes = 2147483648", "disk_reserve_bytes = 1")
        config = config.replace("disk_reserve_percent = 10", "disk_reserve_percent = 1")
        config = config.replace("shutdown_grace_seconds = 60", "shutdown_grace_seconds = 1")
        config_path = base / "server.toml"
        config_path.write_text(config, encoding="utf-8")
        hidden = {"creationflags": subprocess.CREATE_NO_WINDOW} if suffix else {}

        def run(binary, *command, success=True):
            result = subprocess.run([str(binary), "--config", str(config_path), *command],
                                    capture_output=True, text=True, encoding="utf-8", timeout=30, **hidden)
            assert (result.returncode == 0) == success, (command, result.stdout, result.stderr)
            return result

        run(daemon, "check")
        run(daemon, "serve", success=False)
        assert not (base / "mail").exists(), "check/production refusal must not initialize storage"
        run(control, "account", "add", "alice@example.com", "--quota-bytes", "1048576")
        log_path = base / "server.log"

        def start(log):
            process = subprocess.Popen([str(daemon), "--config", str(config_path), "serve-lab"],
                                       stdout=subprocess.DEVNULL, stderr=log, **hidden)
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise AssertionError(f"Server exited: {log_path.read_text(encoding='utf-8')}")
                try:
                    client = smtplib.SMTP("127.0.0.1", port, timeout=5)
                    assert client.ehlo()[0] == 250
                    return process, client
                except (ConnectionRefusedError, TimeoutError, OSError):
                    time.sleep(0.05)
            process.kill()
            process.wait(timeout=10)
            raise AssertionError("Server did not become ready")

        message = EmailMessage(policy=SMTP)
        message["From"] = "sender@remote.test"
        message["To"] = "alice@example.com"
        message["Subject"] = "rustymail independent smoke"
        message.set_content("Hello from Python.\n.leading dot\n中文正文\n")
        raw = message.as_bytes()
        process = None
        client = None
        try:
            with log_path.open("ab") as log:
                process, client = start(log)
                assert "auth" not in client.esmtp_features
                assert "starttls" not in client.esmtp_features
                assert "size" in client.esmtp_features
                assert client.mail("sender@remote.test")[0] == 250
                assert client.rcpt("outside@remote.test")[0] == 550
                assert client.rset()[0] == 250
                assert client.sendmail("sender@remote.test", ["alice@example.com"], raw) == {}
                # sendmail returned only after the receiver's final 250.
                client.close()
                client = None
                process.kill()
                process.wait(timeout=10)
                process = None

            listing = run(control, "mail", "list", "alice@example.com").stdout.splitlines()
            assert len(listing) == 1
            stored = json.loads(listing[0])
            exported = base / "recovered.eml"
            run(control, "mail", "export", "alice@example.com", stored["message_id"], "--output", str(exported))
            assert exported.read_bytes() == raw, "Byte-exact message did not survive forced exit"
            run(control, "mail", "export", "alice@example.com", stored["message_id"], "--output", str(exported), success=False)
            assert exported.read_bytes() == raw, "Existing export must not be overwritten"
            health = json.loads(run(control, "check-store").stdout)
            assert health["healthy"] and health["referenced_blobs"] == 1

            with log_path.open("ab") as log:
                process, client = start(log)
                run(control, "mail", "list", "alice@example.com", success=False)
                assert client.noop()[0] == 250
                client.quit()
                client = None
                process.terminate()
                process.wait(timeout=10)
                process = None
            assert len(run(control, "mail", "list", "alice@example.com").stdout.splitlines()) == 1
            print(json.dumps({"result": "passed", "client": "Python smtplib", "checks": [
                "strict CLI configuration", "production entry refused", "receive-only account",
                "SMTP capability truthfulness", "relay denied", "SMTP final-250 acceptance",
                "forced process termination", "byte-exact recovery/export", "no export overwrite",
                "exclusive admin lock", "server restart"
            ]}, indent=2))
        finally:
            if client is not None:
                client.close()
            if process is not None and process.poll() is None:
                process.kill()
                process.wait(timeout=10)


if __name__ == "__main__":
    main()
