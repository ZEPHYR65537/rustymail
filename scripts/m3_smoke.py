#!/usr/bin/env python3
"""Independent M3.1 role/STARTTLS checks using disposable loopback mail only."""
import argparse
import base64
from collections import Counter
import json
import os
from pathlib import Path
import re
import smtplib
import socket
import ssl
import subprocess
import tempfile
import time
from delivery_assertions import delivery_content

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', default='target/debug')
    parser.add_argument('--output', default='reports/local/m3-smoke.json')
    args = parser.parse_args()
    binaries = Path(args.bin_dir).resolve()
    suffix = '.exe' if os.name == 'nt' else ''
    hidden = {'creationflags': subprocess.CREATE_NO_WINDOW} if suffix else {}
    checks = []
    with tempfile.TemporaryDirectory(prefix='rustymail-m31-') as temporary:
        base = Path(temporary)
        certificates = base/'certificates'
        subprocess.run([str(binaries/'examples'/('m2_certificates'+suffix)), str(certificates)], check=True, **hidden)
        # Reserve simultaneously so all three configured roles have unique ports.
        reservations = [socket.socket() for _ in range(3)]
        try:
            for entry in reservations:
                entry.bind(('127.0.0.1', 0))
            receiver_port, implicit_port, submission_port = [entry.getsockname()[1] for entry in reservations]
        finally:
            for entry in reservations:
                entry.close()
        config = (ROOT/'deploy/rustymail.tls-lab.toml').read_text(encoding='utf-8')
        values = {'data_dir':str(base/'mail'), 'admin_socket':str(base/'admin'/'admin.sock'),
                  'certificate_file':str(certificates/'server.pem'), 'private_key_file':str(certificates/'server.key'),
                  'smtp':f'127.0.0.1:{receiver_port}', 'submissions':f'127.0.0.1:{implicit_port}',
                  'submission':f'127.0.0.1:{submission_port}'}
        for key, value in values.items():
            config = re.sub(r'^'+key+r' = .*$', lambda _:key+' = '+json.dumps(value), config, flags=re.M)
        settings = {'disk_reserve_bytes':1, 'disk_reserve_percent':1, 'connections':4, 'connections_per_ip':4,
                    'ingest_concurrency':4, 'imap_sessions_per_account':4,
                    'tls_handshakes':1, 'handshake_timeout_seconds':3, 'shutdown_grace_seconds':2,
                    'submission_unauthenticated_seconds':30}
        for key, value in settings.items():
            config = re.sub(r'^'+key+r' = .*$', lambda _:f'{key} = {value}', config, flags=re.M)
        config_path = base/'server.toml'
        config_path.write_text(config, encoding='utf-8')
        control = [str(binaries/('rustymailctl'+suffix)), '--config', str(config_path)]
        socket_path = base/'admin'/'admin.sock'

        def ctl(*command, online=False):
            result = subprocess.run(control+(['--socket',str(socket_path)] if online else [])+list(command),
                                    capture_output=True, text=True, encoding='utf-8', timeout=90, **hidden)
            assert result.returncode == 0, (command, result.stderr)
            return json.loads(result.stdout)

        for account in ['alice@example.com', 'bob@example.com']:
            ctl('account', 'add', account)
        ctl('send-as', 'alice@example.com', 'alice@example.com')
        secret_path = base/'credential.secret'
        created = ctl('credential', 'create', 'alice@example.com', '--label', 'm31', '--secret-output', str(secret_path))
        token = secret_path.read_text().strip()
        auth_frame = base64.b64encode(('\0alice@example.com\0'+token).encode()).decode()
        context = ssl.create_default_context(cafile=str(certificates/'ca.pem'))
        raw_receive = b'From: sender@remote.test\r\nTo: bob@example.com\r\nSubject: M3 receive\r\n\r\nSTARTTLS\r\n.dot\r\n'
        raw_submit = b'From: alice@example.com\r\nTo: bob@example.com\r\nSubject: M3 submit\r\n\r\nAuthenticated mail.\r\n'
        log_path = base/'server.log'
        with log_path.open('wb') as log:
            process = None

            def stop():
                nonlocal process
                if process and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                process = None

            def start(lifetime=30):
                nonlocal process
                config_path.write_text(config.replace('submission_unauthenticated_seconds = 30',
                                                     f'submission_unauthenticated_seconds = {lifetime}'), encoding='utf-8')
                process = subprocess.Popen([str(binaries/('rustymaild'+suffix)), '--config', str(config_path), 'serve-lab-smtp'],
                                           stdout=subprocess.DEVNULL, stderr=log, **hidden)
                deadline = time.monotonic()+45
                while True:
                    if process.poll() is not None:
                        raise AssertionError(log_path.read_text(encoding='utf-8'))
                    try:
                        with socket.create_connection(('127.0.0.1',receiver_port),timeout=1):
                            return
                    except OSError:
                        assert time.monotonic() < deadline, 'SMTP listeners did not start'
                        time.sleep(.05)

            def plain(port):
                return smtplib.SMTP('localhost', port, local_hostname='client.example.test', timeout=15)

            def submission():
                client = plain(submission_port)
                assert client.ehlo()[0] == 250
                assert client.starttls(context=context)[0] == 220
                assert client.ehlo()[0] == 250
                return client

            def wrap_after_220(client):
                # Match smtplib.starttls after its STARTTLS command/reply.
                client.sock = context.wrap_socket(client.sock, server_hostname='localhost')
                client.file = None
                client.helo_resp = client.ehlo_resp = None
                client.esmtp_features = {}

            try:
                start()

                with plain(receiver_port) as client:
                    assert client.ehlo()[0] == 250
                    assert client.has_extn('starttls') and not client.has_extn('auth')
                    assert client.docmd('AUTH', 'PLAIN invalid')[0] == 502
                    assert client.mail('sender@remote.test')[0] == 250
                    assert client.rcpt('remote@elsewhere.test')[0] == 550
                    client.rset()
                    assert not client.sendmail('sender@remote.test', ['bob@example.com'], raw_receive)
                    assert client.mail('sender@remote.test')[0] == 250
                    assert client.rcpt('bob@example.com')[0] == 250
                    assert client.starttls(context=context)[0] == 220
                    assert client.docmd('RCPT', 'TO:<bob@example.com>')[0] == 503
                    assert client.docmd('MAIL', 'FROM:<sender@remote.test>')[0] == 503
                    assert client.ehlo()[0] == 250
                    assert not client.has_extn('starttls') and not client.has_extn('auth')
                    assert client.docmd('DATA')[0] == 503
                    assert client.docmd('AUTH', 'PLAIN invalid')[0] == 502
                    assert not client.sendmail('', ['bob@example.com'], raw_receive)
                checks.append('receiver accepts anonymous local mail with/without TLS; upgrade clears greeting and envelope; no AUTH or relay')

                with plain(submission_port) as client:
                    assert client.docmd('STARTTLS')[0] == 503
                    assert client.ehlo()[0] == 250
                    assert client.has_extn('starttls') and not client.has_extn('auth')
                    assert client.docmd('AUTH', 'PLAIN '+auth_frame)[0] == 538
                    assert client.mail('alice@example.com')[0] == 530
                    assert client.docmd('STARTTLS', 'extra')[0] == 501
                    assert client.starttls(context=context)[0] == 220
                    assert client.docmd('AUTH', 'PLAIN '+auth_frame)[0] == 503
                    assert client.ehlo()[0] == 250
                    assert client.has_extn('auth') and not client.has_extn('starttls')
                    assert client.mail('alice@example.com')[0] == 530
                    assert client.login('alice@example.com', token)[0] == 235
                    assert client.docmd('STARTTLS')[0] == 503
                    assert client.mail('bob@example.com')[0] == 553
                    assert client.mail('alice@example.com')[0] == 250
                    assert client.rcpt('remote@elsewhere.test')[0] == 550
                    client.rset()
                    assert not client.sendmail('alice@example.com', ['bob@example.com'], raw_submit)
                checks.append('submission requires STARTTLS, fresh EHLO and AUTH; unauthorized sender/relay/repeated TLS rejected')

                with smtplib.SMTP_SSL('localhost', implicit_port, local_hostname='client.example.test', context=context, timeout=15) as client:
                    assert client.ehlo()[0] == 250
                    assert client.has_extn('auth') and not client.has_extn('starttls')
                    assert client.login('alice@example.com', token)[0] == 235
                    assert not client.sendmail('alice@example.com', ['bob@example.com'], raw_submit)
                checks.append('implicit TLS submission shares account, authorization and store with STARTTLS submission')

                client = plain(receiver_port)
                try:
                    client.ehlo()
                    client.send(b'STARTTLS\r\nEHLO injected\r\nMAIL FROM:<sender@remote.test>\r\nRCPT TO:<bob@example.com>\r\n')
                    assert client.getreply()[0] == 220
                    try:
                        wrap_after_220(client)
                        assert client.docmd('RCPT', 'TO:<bob@example.com>')[0] == 503
                        assert client.ehlo()[0] == 250
                        assert client.docmd('DATA')[0] == 503
                    except (ssl.SSLError, ConnectionError, smtplib.SMTPServerDisconnected):
                        # Bytes not prefetched by BufReader hit TLS decoding and
                        # close the connection; they must never execute as SMTP.
                        pass
                finally:
                    client.close()
                checks.append('pipelined plaintext cannot cross STARTTLS as SMTP commands (discard or close)')

                client = plain(submission_port)
                try:
                    client.ehlo()
                    try:
                        client.starttls(context=ssl.create_default_context())
                    except ssl.SSLCertVerificationError:
                        pass
                    else:
                        raise AssertionError('untrusted STARTTLS certificate accepted')
                finally:
                    client.close()
                client = plain(submission_port)
                try:
                    client.ehlo()
                    assert client.docmd('STARTTLS')[0] == 220
                    client.send(b'EHLO plaintext-after-220\r\n')
                    try:
                        code, _ = client.getreply()
                    except (OSError, smtplib.SMTPServerDisconnected):
                        pass
                    else:
                        # A TLS alert is binary; smtplib may return -1 instead
                        # of raising when its line reader sees alert + EOF.
                        assert code == -1, 'failed TLS fell back to SMTP'
                        try:
                            assert not client.sock.recv(1)
                        except ConnectionResetError:
                            pass
                finally:
                    client.close()
                checks.append('verified STARTTLS rejects unknown CA; handshake failure never falls back to plaintext SMTP')

                stalled = plain(submission_port)
                try:
                    stalled.ehlo()
                    assert stalled.docmd('STARTTLS')[0] == 220
                    with plain(receiver_port) as another:
                        another.ehlo()
                        assert another.docmd('STARTTLS')[0] == 454
                        assert another.noop()[0] == 250
                    stalled.sock.settimeout(6)
                    try:
                        assert not stalled.sock.recv(1)
                    except ConnectionResetError:
                        pass
                finally:
                    stalled.close()
                with submission() as client:
                    assert client.noop()[0] == 250
                checks.append('global handshake budget applies across ports; stalled handshakes expire and return their permit')

                holders = []
                try:
                    holders.extend([plain(receiver_port), plain(submission_port), plain(receiver_port)])
                    holders.append(smtplib.SMTP_SSL('localhost', implicit_port, local_hostname='client.example.test', context=context, timeout=15))
                    for client in holders:
                        assert client.noop()[0] == 250, 'holder expired before admission probe'
                    try:
                        extra = plain(receiver_port)
                    except smtplib.SMTPConnectError as error:
                        assert error.smtp_code == 421
                    else:
                        extra.close()
                        raise AssertionError('ports bypassed the global connection limit')
                finally:
                    for client in holders:
                        client.close()
                checks.append('connection admission is shared by all three ports')

                if os.name != 'nt':
                    assert ctl('status', online=True)['mode'] == 'lab_smtp'
                    client = submission()
                    try:
                        client.login('alice@example.com', token)
                        client.mail('alice@example.com')
                        client.rcpt('bob@example.com')
                        assert client.docmd('DATA')[0] == 354
                        ctl('credential', 'revoke', created['selector'], online=True)
                        assert client.getreply()[0] == 421
                    finally:
                        client.close()
                    checks.append('online revocation terminates an upgraded submission during DATA')
                # Keep the short lifetime out of unrelated admission/AUTH tests.
                stop()
                start(lifetime=5)
                client = plain(submission_port)
                try:
                    client.ehlo()
                    time.sleep(3)
                    client.starttls(context=context)
                    client.ehlo()
                    client.sock.settimeout(3)
                    assert client.getreply()[0] == 421
                finally:
                    client.close()
                checks.append('STARTTLS does not restart the original five-second unauthenticated lifetime')
            finally:
                stop()

        integrity = ctl('check-store')
        assert integrity['healthy'] and integrity['referenced_blobs'] == 4, integrity
        result = subprocess.run(control+['mail','list','bob@example.com'],capture_output=True,text=True,check=True,**hidden)
        messages = [json.loads(line) for line in result.stdout.splitlines()]
        assert len(messages) == 4
        exported = []
        trace_protocols = Counter()
        for index, message in enumerate(messages):
            path = base/f'accepted-{index}.eml'
            ctl('mail','export','bob@example.com',message['message_id'],'--output',str(path))
            blob = path.read_bytes()
            sender = ('alice@example.com' if blob.startswith(b'Return-Path: <alice@example.com>')
                      else '' if blob.startswith(b'Return-Path: <>') else 'sender@remote.test')
            content, _ = delivery_content(blob, sender)
            trace_protocols[(sender, blob.split(b'\r\n', 3)[2].split(b' with ')[1])] += 1
            assert message['size_bytes'] == len(blob)
            exported.append(content)
        assert Counter(exported) == Counter([raw_receive, raw_receive, raw_submit, raw_submit])
        assert trace_protocols == Counter({('sender@remote.test', b'ESMTP'): 1,
                                          ('', b'ESMTPS'): 1, ('alice@example.com', b'ESMTPSA'): 2})
        checks.append('restart preserves exactly four accepted messages with valid final trace and byte-exact retained content; all negatives leave no accepted message')
        logs = log_path.read_text(encoding='utf-8')
        assert token not in logs and auth_frame not in logs and '$argon2id$' not in logs
        assert 'Authenticated mail.' not in logs and 'EHLO injected' not in logs
        checks.append('diagnostic logs contain no application password, AUTH frame or test message body')
    report = {'revision':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),
              'working_tree_dirty':bool(subprocess.check_output(['git','status','--porcelain'],text=True).strip()),
              'platform':os.name,'openssl':ssl.OPENSSL_VERSION,'checks':checks,'production_ready':False}
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(report,indent=2))


if __name__ == '__main__':
    main()
