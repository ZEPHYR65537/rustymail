"""Disposable three-role SMTP fixture shared by contract and pressure experiments.

Uses public CLI/TCP interfaces only; no Rust implementation imports or mock storage.
"""
import json
import os
from pathlib import Path
import platform
import re
import smtplib
import socket
import ssl
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


class LoopbackSMTP(smtplib.SMTP):
    def _get_socket(self, host, port, timeout):
        # Bindings are IPv4 only. Avoid platform-dependent localhost IPv6
        # fallback delays while keeping TLS verification against localhost.
        return socket.create_connection(('127.0.0.1', port), timeout)


class LoopbackSMTPSSL(smtplib.SMTP_SSL):
    def _get_socket(self, host, port, timeout):
        stream = socket.create_connection(('127.0.0.1', port), timeout)
        try:
            return self.context.wrap_socket(stream, server_hostname=host)
        except BaseException:
            stream.close()
            raise


def report(output, **values):
    values.update(revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                  working_tree_dirty=bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT, text=True).strip()),
                  platform=platform.platform(), production_ready=False)
    destination = Path(output).resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(json.dumps(values, indent=2)+'\n', encoding='utf-8')
    print(json.dumps(values))


class Lab:
    command = 'serve-lab-smtp'

    def configure(self, text, certificates):
        return text

    def __init__(self, bin_dir, settings=None):
        self.binaries = Path(bin_dir).resolve()
        self.settings = settings or {}
        self.process = None
        self.log = None
        self.suffix = '.exe' if os.name == 'nt' else ''
        self.hidden = {'creationflags': subprocess.CREATE_NO_WINDOW} if self.suffix else {}

    def ctl(self, *args):
        return subprocess.run([str(self.binaries/('rustymailctl'+self.suffix)), '--config', str(self.config), *args],
                              capture_output=True, text=True, encoding='utf-8', check=True, timeout=90, **self.hidden).stdout

    def __enter__(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='rustymail-smtp-')
        try:
            self.base = Path(self.temporary.name)
            certs = self.base/'certificates'
            subprocess.run([str(self.binaries/'examples'/('m2_certificates'+self.suffix)), str(certs)],
                           capture_output=True, check=True, timeout=20, **self.hidden)
            reservations = [socket.socket() for _ in range(3)]
            try:
                for sock in reservations:
                    sock.bind(('127.0.0.1', 0))
                self.ports = dict(zip(['smtp', 'submissions', 'submission'], [s.getsockname()[1] for s in reservations]))
            finally:
                for sock in reservations:
                    sock.close()
            values = {'data_dir':str(self.base/'mail'), 'admin_socket':str(self.base/'admin'/'admin.sock'),
                      'certificate_file':str(certs/'server.pem'), 'private_key_file':str(certs/'server.key'),
                      **{key:f'127.0.0.1:{port}' for key, port in self.ports.items()},
                      'disk_reserve_bytes':1, 'disk_reserve_percent':1, 'connections':8, 'connections_per_ip':8,
                      'imap_sessions_per_account':4, 'ingest_concurrency':2, 'tls_handshakes':2,
                      'message_bytes':65536, 'header_bytes':4096, 'recipients_per_message':1,
                      'temporary_reserved_bytes':135168, 'shutdown_grace_seconds':1,
                      'smtp_command_seconds':30, 'data_idle_seconds':3, 'data_total_seconds':6,
                      'handshake_timeout_seconds':3, 'submission_unauthenticated_seconds':30, **self.settings}
            text = (ROOT/'deploy/rustymail.tls-lab.toml').read_text(encoding='utf-8')
            for key, value in values.items():
                text, count = re.subn(r'^'+key+r' = .*$', lambda _:f'{key} = {json.dumps(value)}', text, count=1, flags=re.M)
                assert count == 1, (key, count)
            self.config = self.base/'server.toml'
            text = self.configure(text, certs)
            self.config.write_text(text, encoding='utf-8')
            for address in ['alice@example.com', 'bob@example.com', 'postmaster@example.com']:
                self.ctl('account', 'add', address)
            self.ctl('send-as', 'alice@example.com', 'alice@example.com')
            secret = self.base/'credential.secret'
            self.ctl('credential', 'create', 'alice@example.com', '--label', 'contract', '--secret-output', str(secret))
            self.token = secret.read_text().strip()
            self.context = ssl.create_default_context(cafile=str(certs/'ca.pem'))
            self.log_path = self.base/'daemon.log'
            self.log = self.log_path.open('wb')
            self.process = subprocess.Popen([str(self.binaries/('rustymaild'+self.suffix)), '--config', str(self.config), self.command],
                                            stdout=subprocess.DEVNULL, stderr=self.log, **self.hidden)
            deadline = time.monotonic()+30
            while True:
                try:
                    with self.connect() as client:
                        assert client.noop()[0] == 250
                    break
                except OSError:
                    assert self.process.poll() is None and time.monotonic() < deadline, self.log_path.read_text()
                    time.sleep(.05)
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def connect(self, role='smtp', authenticate=False, starttls=True):
        if role == 'submissions':
            client = LoopbackSMTPSSL('localhost', self.ports[role], context=self.context, timeout=15)
        else:
            client = LoopbackSMTP('localhost', self.ports[role], local_hostname='client.test', timeout=15)
        try:
            assert client.ehlo('client.test')[0] == 250
            if role == 'submission' and starttls:
                assert client.starttls(context=self.context)[0] == 220
                assert client.ehlo('client.test')[0] == 250
            if authenticate:
                assert client.login('alice@example.com', self.token)[0] == 235
            return client
        except BaseException:
            client.close()
            raise

    def stop(self):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
                raise AssertionError('daemon failed to stop within grace period')

    def check_store(self, count):
        self.stop()
        staging = self.base/'mail'/'staging'
        assert staging.is_dir() and not list(staging.iterdir()), 'abandoned staging files'
        integrity = json.loads(self.ctl('check-store'))
        assert integrity['healthy'] and integrity['referenced_blobs'] == count, integrity
        logs = self.log_path.read_text(encoding='utf-8')
        assert self.token not in logs
        return integrity

    def __exit__(self, *_):
        try:
            self.stop()
        finally:
            if self.log:
                self.log.close()
            self.temporary.cleanup()
