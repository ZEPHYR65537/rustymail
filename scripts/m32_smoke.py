#!/usr/bin/env python3
"""M3.2 final-delivery checks against a disposable store and independent client."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import smtplib
import socket
import sqlite3
import subprocess
import tempfile
import time
from delivery_assertions import delivery_content

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', default='target/debug')
    parser.add_argument('--output', default='reports/local/m32-smoke.json')
    args = parser.parse_args()
    binaries = Path(args.bin_dir).resolve()
    suffix = '.exe' if os.name == 'nt' else ''
    hidden = {'creationflags': subprocess.CREATE_NO_WINDOW} if suffix else {}
    checks = []
    with tempfile.TemporaryDirectory(prefix='rustymail-m32-') as temporary:
        base = Path(temporary)
        with socket.socket() as reservation:
            reservation.bind(('127.0.0.1', 0))
            port = reservation.getsockname()[1]
        config = (ROOT/'deploy/rustymail.lab.toml').read_text(encoding='utf-8')
        settings = {'data_dir':str(base/'mail'), 'smtp':f'127.0.0.1:{port}',
                    'message_bytes':4096, 'header_bytes':1024, 'temporary_reserved_bytes':6144,
                    'disk_reserve_bytes':1, 'disk_reserve_percent':1, 'shutdown_grace_seconds':1}
        for key, value in settings.items():
            config = re.sub(r'^'+key+r' = .*$', lambda _:f'{key} = {json.dumps(value)}', config, flags=re.M)
        config_path = base/'server.toml'
        config_path.write_text(config, encoding='utf-8')
        control = [str(binaries/('rustymailctl'+suffix)), '--config', str(config_path)]

        def ctl(*command):
            result = subprocess.run(control+list(command), capture_output=True, text=True,
                                    encoding='utf-8', timeout=30, check=True, **hidden)
            return result.stdout

        alice, bob, tiny = 'alice@example.com', 'bob@example.com', 'tiny@example.com'
        for account in [alice, bob]:
            ctl('account', 'add', account)
        ctl('account', 'add', tiny, '--quota-bytes', '100')
        expected = {}
        log_path = base/'server.log'
        with log_path.open('wb') as log:
            process = subprocess.Popen([str(binaries/('rustymaild'+suffix)), '--config', str(config_path), 'serve-lab'],
                                       stdout=subprocess.DEVNULL, stderr=log, **hidden)
            try:
                deadline = time.monotonic() + 20
                while True:
                    try:
                        probe = smtplib.SMTP('127.0.0.1', port, local_hostname='client.example.test', timeout=5)
                        probe.quit()
                        break
                    except OSError:
                        if process.poll() is not None or time.monotonic() > deadline:
                            raise AssertionError(log_path.read_text())
                        time.sleep(.05)

                def connect(helo=None):
                    client = smtplib.SMTP('127.0.0.1', port, local_hostname='client.example.test', timeout=10)
                    response = client.helo(helo) if helo else client.ehlo()
                    assert response[0] == 250
                    if not helo:
                        assert client.esmtp_features['size'] == '4096'
                    return client

                def envelope(client, sender, recipients):
                    assert client.mail(sender, options=['SIZE=1'] if client.does_esmtp else [])[0] == 250
                    for recipient in recipients:
                        assert client.rcpt(recipient)[0] == 250

                def send(raw, sender='', recipients=(alice,), retained=None, helo=None):
                    with connect(helo) as client:
                        envelope(client, sender, recipients)
                        code, response = client.data(raw)
                        assert code == 250, (code, response)
                        message_id = response.decode().split()[-1]
                    expected[message_id] = (raw if retained is None else retained, sender,
                                            set(r.lower() for r in recipients), 'SMTP' if helo else 'ESMTP')
                    return message_id

                retained = (b'Received: from previous.test\r\n\tby old.test; old-date\r\n'
                            b'DKIM-Signature: opaque-lab-fixture\r\n\tb=unchanged;\r\n'
                            b'From: author@remote.test\r\nSubject: trace test\r\n\r\n'
                            b'.leading dot\r\nReturn-Path: body unchanged\r\n\xffopaque body\r\n')
                raw = (b'return-path: <forged@remote.test>\r\n\tfake continuation\r\n'
                       b'RETURN-PATH: <>\r\n' + retained)
                first = send(raw, 'Bounce@remote.test', [alice, bob, 'ALICE@example.com'], retained)
                second = send(raw, 'Bounce@remote.test', retained=retained)
                assert first != second  # External retransmission is a new SMTP transaction.
                checks.append('local shared blob; duplicate RCPT deduplication; folded old Return-Path removed; prior Received and body retained')
                exact = b'\r\n' + (b'x'*998+b'\r\n')*4 + b'y'*92+b'\r\n'
                assert len(exact) == 4096
                send(exact)
                send(b'Subject: null sender\r\n\r\nnull reverse path\r\n')
                send(b'', retained=b'\r\n')
                send(b'Subject: no separator\r\n', retained=b'Subject: no separator\r\n\r\n')
                send(b'\r\nHELO body\r\n', helo='untrusted);for<victim>')
                checks.append('SIZE counts decoded client bytes; exact limit accepted with trace overhead; null sender, empty and header-only DATA handled')

                bad_headers = [b' orphan\r\n\r\n', b'Return-Path : <>\r\n\r\n',
                               b'X: \x01\r\n\r\n', b'not a header\r\n\r\n']
                cases = [(raw, 550) for raw in bad_headers]
                cases += [(exact + b'overflow\r\n', 552),
                          (b'Return-Path: <>\r\n'+(b' '+b'x'*98+b'\r\n')*11+b'\r\n', 552)]
                for raw, expected_code in cases:
                    client = connect()
                    try:
                        envelope(client, '', [alice])
                        assert client.data(raw)[0] == expected_code
                        try:
                            client.noop()
                        except (smtplib.SMTPServerDisconnected, ConnectionError, OSError):
                            pass
                        else:
                            raise AssertionError('invalid DATA connection was retained')
                    finally:
                        client.close()
                checks.append('invalid header boundaries rejected; actual oversize and removed-header bytes count toward limits; connection closes')
                with connect() as client:
                    envelope(client, '', [alice, tiny])
                    assert client.data(b'Subject: small\r\n\r\nsmall\r\n')[0] == 452
                    assert client.noop()[0] == 250
                checks.append('generated trace counts toward quota; one recipient over quota rejects all recipients atomically')

                holder, blocked = connect(), connect()
                try:
                    envelope(holder, '', [alice])
                    holder.putcmd('DATA')
                    assert holder.getreply()[0] == 354
                    envelope(blocked, '', [alice])
                    blocked.putcmd('DATA')
                    assert blocked.getreply()[0] == 452
                finally:
                    holder.close()
                    blocked.close()
                checks.append('6144-byte temporary budget admits one 4096-byte input plus 2048-byte trace reservation')
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)

        integrity = json.loads(ctl('check-store'))
        assert integrity['healthy'] and integrity['referenced_blobs'] == len(expected)
        exports, operations = {}, set()
        for account in [alice, bob, tiny]:
            listing = [json.loads(line) for line in ctl('mail', 'list', account).splitlines()]
            assert {m['message_id'] for m in listing} == {mid for mid, entry in expected.items() if account in entry[2]}
            for message in listing:
                mid = message['message_id']
                destination = base/(account+'-'+mid+'.eml')
                ctl('mail', 'export', account, mid, '--output', str(destination))
                blob = destination.read_bytes()
                content, operation = delivery_content(blob, expected[mid][1], expected[mid][3])
                assert content == expected[mid][0]
                assert message['size_bytes'] == len(blob)
                if mid in exports:
                    assert blob == exports[mid]
                else:
                    assert operation not in operations
                    operations.add(operation)
                    exports[mid] = blob
                record = json.loads(ctl('operation', operation))['operation']
                assert record['message_id'] == mid
        connection = sqlite3.connect(base/'mail/meta.sqlite')
        try:
            for account, used in connection.execute('SELECT login,used_bytes FROM account'):
                assert used == sum(len(exports[mid]) for mid, value in expected.items() if account in value[2])
            for size, digest, blob_id in connection.execute('SELECT size_bytes,sha256,id FROM blob'):
                stored = (base/'mail'/'blobs'/(blob_id+'.eml')).read_bytes()
                assert size == len(stored) and digest == hashlib.sha256(stored).hexdigest()
        finally:
            connection.close()
        checks.append('reopened exports, shared recipient bytes, quota, digest and operation lookup match the final representation')
        logs = log_path.read_text(encoding='utf-8')
        assert 'opaque body' not in logs and 'forged@remote.test' not in logs
        report = {'revision':subprocess.check_output(['git','rev-parse','HEAD'], cwd=ROOT, text=True).strip(),
                  'working_tree_dirty':bool(subprocess.check_output(['git','status','--porcelain'], cwd=ROOT, text=True).strip()),
                  'platform':platform.platform(), 'checks':checks, 'accepted_messages':len(expected), 'production_ready':False}
        output = Path(args.output).resolve()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, indent=2)+'\n', encoding='utf-8')
        print(json.dumps(report))


if __name__ == '__main__':
    main()
