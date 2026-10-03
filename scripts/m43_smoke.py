#!/usr/bin/env python3
"""M4.3 independent PIPELINING, DSN, administration and process-kill checks."""
import argparse
from contextlib import closing
from email import policy
from email.parser import BytesParser
import json
import os
from pathlib import Path
import queue
import re
import select
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
from m42_smoke import Peer, RelayLab, configuration
from smtp_lab import Lab, report


class PipelinePeer(Peer):
    def session(self, stream):
        secured = self.mode == 'implicit'
        if secured:
            stream = self.context.wrap_socket(stream, server_side=True)
        record = {'commands': [], 'body': None, 'group_before_replies': False}
        self.records.append(record)

        def line():
            data = bytearray()
            while not data.endswith(b'\r\n'):
                byte = stream.recv(1)
                if not byte:
                    return bytes(data)
                data.extend(byte)
                assert len(data) <= 2048
            return bytes(data)

        stream.sendall(b'220 pipeline.test\r\n')
        while True:
            command = line()
            if not command:
                return
            record['commands'].append(command.decode('ascii').strip())
            if command.startswith(b'EHLO '):
                enabled = self.scenario != 'sequential' and not (self.scenario == 'reset' and secured)
                extensions = b'250-PIPELINING\r\n' if enabled else b''
                if not secured and self.mode == 'starttls':
                    extensions += b'250-STARTTLS\r\n'
                stream.sendall(b'250-pipeline.test\r\n'+extensions+b'250-8BITMIME\r\n250 SIZE 30000000\r\n')
            elif command == b'STARTTLS\r\n':
                stream.sendall(b'220 upgrade\r\n')
                stream = self.context.wrap_socket(stream, server_side=True)
                secured = True
            elif command.startswith(b'MAIL FROM:'):
                if enabled:
                    rcpt, data = line(), line()
                    assert rcpt.startswith(b'RCPT TO:') and data == b'DATA\r\n', (rcpt, data)
                    record['commands'] += [rcpt.decode().strip(), 'DATA']
                    record['group_before_replies'] = True
                    scenario = self.scenario
                    if scenario == 'close':
                        return
                    if scenario == 'missing':
                        stream.sendall(b'250 sender\r\n')
                        self.stop.wait(3)
                        return
                    replies = {
                        'mail4': b'450 sender\r\n250 recipient\r\n354 body\r\n',
                        'mail5': b'550 sender\r\n250 recipient\r\n354 body\r\n',
                        'rcpt4': b'250 sender\r\n450 recipient\r\n354 body\r\n',
                        'rcpt5': b'250 sender\r\n550 recipient\r\n354 body\r\n',
                        'data5': b'250 sender\r\n250 recipient\r\n554 rejected\r\n',
                        '421': b'421 closing\r\n',
                        'malformed': b'250-sender\r\n550 mismatched\r\n',
                        'extra': b'250 sender\r\n250 recipient\r\n354 body\r\n250 unsolicited\r\n',
                    }.get(scenario, b'250-sender continuation\r\n250 sender\r\n250 recipient\r\n354 body\r\n')
                    if scenario == 'fragmented':
                        for byte in replies:
                            stream.sendall(bytes([byte]))
                    else:
                        stream.sendall(replies)
                    if scenario in ('mail4', 'mail5', 'rcpt4', 'rcpt5', 'data5', '421', 'malformed', 'extra'):
                        record['unexpected_body'] = stream.recv(1024)
                        assert not record['unexpected_body'], record
                        return
                else:
                    # No buffered reader: pending TLS bytes / socket readiness expose
                    # a client incorrectly retaining the pre-STARTTLS capability.
                    assert not (secured and stream.pending()), record
                    assert not select.select([stream], [], [], .05)[0], record
                    stream.sendall(b'250 sender\r\n')
                    assert line().startswith(b'RCPT TO:')
                    stream.sendall(b'250 recipient\r\n')
                    assert line() == b'DATA\r\n'
                    stream.sendall(b'354 body\r\n')
                body = bytearray()
                while True:
                    part = line()
                    if not part:
                        return
                    if part == b'.\r\n':
                        break
                    body.extend(part[1:] if part.startswith(b'..') else part)
                record['body'] = bytes(body)
                if self.scenario != 'lost_final':
                    stream.sendall(b'250 accepted\r\n')
                return
            else:
                raise AssertionError(command)


def await_notice(lab, state='created', error=None):
    deadline = time.monotonic()+15
    while True:
        with closing(sqlite3.connect(lab.base/'mail/meta.sqlite')) as db:
            rows = db.execute("SELECT notification_state,notification_error FROM delivery WHERE state='failed'").fetchall()
        if rows and all(r[0] == state and (error is None or r[1] == error) for r in rows):
            return
        assert time.monotonic() < deadline, (rows, lab.log_path.read_text())
        time.sleep(.1)


def read_report(lab, status='5.0.0'):
    with closing(sqlite3.connect(lab.base/'mail/meta.sqlite')) as db:
        rows = db.execute("SELECT m.reverse_path,m.source,m.blob_id,n.delivery_id FROM notification n JOIN message m ON m.id=n.report_message_id").fetchall()
        assert len(rows) == 1 and rows[0][:2] == ('', 'dsn'), rows
        assert db.execute('SELECT count(*) FROM mailbox_message').fetchone()[0] == 1
        assert db.execute("SELECT uidnext FROM mailbox WHERE name='INBOX' AND account_id=(SELECT id FROM account WHERE login=?)", ('alice@example.com',)).fetchone()[0] == 2
    raw = (lab.base/'mail/blobs'/(rows[0][2]+'.eml')).read_bytes()
    assert len(raw) <= 16384 and b'PRIVATE' not in raw and b'Bcc' not in raw
    assert b'\n' not in raw.replace(b'\r\n', b'')
    parsed = BytesParser(policy=policy.default).parsebytes(raw)
    assert not parsed.defects and parsed.get_content_type() == 'multipart/report'
    assert parsed.get_param('report-type') == 'delivery-status'
    assert parsed['Return-Path'] == '<>' and parsed['Auto-Submitted'] == 'auto-generated'
    assert str(parsed['To']) == 'alice@example.com'
    blocks = parsed.get_payload(1).get_payload()
    assert blocks[0]['Reporting-MTA'].startswith('dns; ')
    assert blocks[1]['Final-Recipient'] == 'rfc822; fail@remote.test'
    assert blocks[1]['Action'] == 'failed' and blocks[1]['Status'] == status
    return rows[0][3]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', default='target/debug')
    parser.add_argument('--output', default='reports/local/m43-smoke.json')
    args = parser.parse_args()
    binaries = Path(args.bin_dir).resolve()
    suffix = '.exe' if os.name == 'nt' else ''
    hidden = {'creationflags': subprocess.CREATE_NO_WINDOW} if suffix else {}
    checks, attempts, cuts = [], [], []

    def run(command):
        result = subprocess.run([str(x) for x in command], capture_output=True, text=True, encoding='utf-8', timeout=30, **hidden)
        assert result.returncode == 0, (command, result.stdout, result.stderr)
        return result.stdout

    with tempfile.TemporaryDirectory(prefix='rustymail-lifecycle-') as temporary:
        base = Path(temporary)
        certificates = base/'certificates'
        run([binaries/'examples'/('m2_certificates'+suffix), certificates])
        source = base/'message.eml'
        raw = b'From: sender@example.com\r\nSubject: pipeline\r\n\r\n.dot\r\n\xff\r\n'
        source.write_bytes(raw)
        cases = [('ok', {'Delivered':250}, True), ('fragmented', {'Delivered':250}, True),
                 ('mail4', {'Temporary':450}, False), ('mail5', {'Permanent':550}, False),
                 ('rcpt4', {'Temporary':450}, False), ('rcpt5', {'Permanent':550}, False),
                 ('data5', {'Permanent':554}, False), ('421', {'Temporary':421}, False),
                 ('malformed', 'ConnectionLost', False), ('extra', 'ConnectionLost', False),
                 ('missing', 'ConnectionLost', False), ('close', 'ConnectionLost', False),
                 ('lost_final', 'ConnectionLost', True), ('sequential', {'Delivered':250}, True),
                 ('reset', {'Delivered':250}, True)]
        for scenario, expected, marked in cases:
            with closing(PipelinePeer(certificates, 'starttls' if scenario == 'reset' else 'implicit', scenario=scenario)) as peer:
                config = configuration(base, peer, certificates)
                observed = json.loads(run([binaries/'examples'/('m42_attempt'+suffix), config, source, 'target@remote.test', 'tls']))
                assert observed == {'result':expected, 'body_marked':marked}, (scenario, observed, peer.records)
                assert peer.records[0]['group_before_replies'] == (scenario not in ('sequential', 'reset'))
                if marked:
                    assert peer.records[0]['body'] == raw
                attempts.append({'scenario':scenario, **observed})
        checks.append('L01: 15 verified-TLS peer cases: complete command group before replies, fragmentation/coalescing, positional failures, malformed/extra/missing replies, lost final and post-STARTTLS capability reset')

        probe = binaries/'examples'/('m43_probe'+suffix)
        for boundary in ('file', 'directories', 'prepared', 'before', 'after', 'returned'):
            root = base/('cut-'+boundary)
            run([probe, root, 'seed', 'none'])
            child = subprocess.Popen([str(probe), str(root), 'notify', boundary], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, **hidden)
            signals = queue.Queue()
            reader = threading.Thread(target=lambda: signals.put(child.stdout.readline()), daemon=True)
            reader.start()
            try:
                assert signals.get(timeout=15).strip() == 'RUSTYMAIL_NOTIFICATION_CUT'
                child.kill()
                child.wait(timeout=10)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait(timeout=10)
                reader.join(timeout=5)
                child.stdout.close()
                child.stderr.close()
            with closing(sqlite3.connect(root/'meta.sqlite')) as db:
                before = db.execute('SELECT count(*) FROM notification').fetchone()[0]
                assert before == int(boundary in ('after', 'returned'))
            replay = json.loads(run([probe, root, 'notify', 'none']))
            run([probe, root, 'notify', 'none'])
            with closing(sqlite3.connect(root/'meta.sqlite')) as db:
                assert db.execute('SELECT count(*) FROM notification').fetchone()[0] == 1
                assert db.execute('SELECT uid FROM mailbox_message').fetchall() == [(1,)]
                assert db.execute('PRAGMA foreign_key_check').fetchall() == []
            cuts.append({'boundary':boundary, 'reports_before_recovery':before, 'reports_after_replay':1, 'integrity':replay['integrity']})
        checks.append('L02: six real process kills at file sync, publication, preparation, before/after commit and after return; replay creates one notification and one UID')

    with Lab(args.bin_dir) as lab:
        with lab.connect() as client:
            assert 'pipelining' in client.esmtp_features and 'dsn' not in client.esmtp_features
            client.send(b'MAIL FROM:<sender@remote.test>\r\nRCPT TO:<bob@example.com>\r\nDATA\r\n')
            assert [client.getreply()[0] for _ in range(3)] == [250, 250, 354]
            client.send(b'From: sender@remote.test\r\n\r\nRSET\r\n..dot\r\n.\r\nNOOP\r\n')
            assert [client.getreply()[0] for _ in range(2)] == [250, 250]
            group = b'MAIL FROM:<sender@remote.test>\r\nRCPT TO:<missing@example.com>\r\nDATA\r\nRSET\r\nNOOP\r\n'
            for byte in group:
                client.send(bytes([byte]))
            assert [client.getreply()[0] for _ in range(5)] == [250, 550, 503, 250, 250]
        lab.check_store(1)
    checks.append('L03: independent receiver pipelining, byte fragmentation, DATA/command boundary, failed recipient and RSET ordering; no DSN extension advertised')

    for quota in (False, True):
        with RelayLab(args.bin_dir) as lab:
            if quota:
                lab.stop()
                lab.ctl('account', 'quota', 'alice@example.com', '0')
                lab.restart()
            with lab.connect('submissions', authenticate=True) as client:
                assert client.sendmail('alice@example.com', ['fail@remote.test'], b'From: alice@example.com\r\nBcc: PRIVATE@hidden.test\r\n\r\nPRIVATE\r\n') == {}
            lab.await_states({'fail@remote.test':'failed'})
            await_notice(lab, 'pending' if quota else 'created', 'recipient_quota' if quota else None)
            lab.stop()
            if quota:
                with closing(sqlite3.connect(lab.base/'mail/meta.sqlite')) as db:
                    assert db.execute('SELECT count(*) FROM notification').fetchone()[0] == 0
                    assert db.execute('SELECT count(*) FROM blob').fetchone()[0] == 1
                    # Only this stopped disposable fixture: make the persisted
                    # retry due now. The full 60-second gate is unit-tested.
                    db.execute('UPDATE delivery SET notification_due_ms=0')
                    db.commit()
                lab.ctl('account', 'quota', 'alice@example.com', '100000')
                maintained = json.loads(lab.ctl('queue', 'maintain', '--limit', '1'))
                assert maintained['reports_created'] == 1 and not maintained['transmitted']
            delivery = read_report(lab)
            shown = json.loads(lab.ctl('queue', 'show', delivery))
            assert shown['delivery']['notification_state'] == 'created'
            assert len(shown['report_deliveries']) == 1 and shown['report_deliveries'][0]['state'] == 'delivered'
            lab.ctl('queue', 'maintain')
            lab.restart()
            time.sleep(1.2)
            lab.check_store(2)
            read_report(lab)
    checks.append('L04: real authenticated submission -> permanent relay failure -> private MIME DSN delivered locally once; quota blockage retains original responsibility and offline maintenance recovers')

    for report_fails in (False, True):
        with RelayLab(args.bin_dir) as lab:
            lab.stop()
            # Management only grants local send-as. This disposable fixture
            # models an existing grant after its domain leaves local_domains.
            # No production authorization rule is bypassed or weakened.
            original_config = lab.config.read_text(encoding='utf-8')
            local_config, replaced = re.subn(r'^local_domains = .*$', 'local_domains = ["example.com", "remote.test"]', original_config, count=1, flags=re.M)
            assert replaced == 1
            lab.config.write_text(local_config, encoding='utf-8')
            lab.ctl('send-as', 'alice@example.com', 'Owner@remote.test')
            lab.config.write_text(original_config, encoding='utf-8')
            if report_fails:
                lab.peer.scenario = 'rcpt5'
            lab.restart()
            with lab.connect('submissions', authenticate=True) as client:
                client.sendmail('Owner@remote.test', ['fail@remote.test'], b'From: alice@example.com\r\n\r\nPRIVATE\r\n')
            lab.await_states({'fail@remote.test':'failed', 'Owner@remote.test':'failed' if report_fails else 'delivered'})
            if report_fails:
                deadline = time.monotonic()+15
                while True:
                    with closing(sqlite3.connect(lab.base/'mail/meta.sqlite')) as db:
                        state = db.execute("SELECT d.notification_state FROM delivery d JOIN message m ON m.id=d.message_id WHERE m.source='dsn'").fetchone()[0]
                    if state == 'suppressed':
                        break
                    assert time.monotonic() < deadline
                    time.sleep(.1)
            lab.check_store(2)
            with closing(sqlite3.connect(lab.base/'mail/meta.sqlite')) as db:
                assert db.execute('SELECT count(*) FROM notification').fetchone()[0] == 1
                assert db.execute('SELECT count(*) FROM mailbox_message').fetchone()[0] == 0
            records = [r for r in lab.peer.records if r.get('recipient') == 'Owner@remote.test']
            assert len(records) == 1 and any(c.startswith('MAIL FROM:<>') for c in records[0]['commands'])
            if not report_fails:
                body = records[0]['body']
                assert b'PRIVATE' not in body and not body.startswith(b'Return-Path:')
                parsed = BytesParser(policy=policy.default).parsebytes(body)
                assert str(parsed['To']) == 'Owner@remote.test' and parsed.get_content_type() == 'multipart/report'
    checks.append('L05: authorized external reverse-path is preserved on real TLS; DSN uses MAIL FROM:<>; its own 550 is queryable and cannot generate another report')

    with RelayLab(args.bin_dir) as lab:
        lab.peer.scenario = 'rcpt4'
        with lab.connect('submissions', authenticate=True) as client:
            client.sendmail('alice@example.com', ['fail@remote.test'], b'From: alice@example.com\r\n\r\nPRIVATE\r\n')
        lab.await_states({'fail@remote.test':'deferred'})
        lab.stop()
        with closing(sqlite3.connect(lab.base/'mail/meta.sqlite')) as db:
            # Owned, stopped fixture, simulating expiry with a much later retry.
            db.execute("UPDATE delivery SET expires_at_ms=?,next_attempt_at_ms=? WHERE route='relay'", (int(time.time()*1000)-1, int(time.time()*1000)+86400000))
            db.commit()
        result = json.loads(lab.ctl('queue', 'maintain', '--limit', '1'))
        assert result['expired'] == 1 and result['reports_created'] == 1
        read_report(lab, '5.4.7')
        lab.check_store(2)
    checks.append('L06: expiry scan is independent of future next_attempt_at_ms; DSN delivery-status reports 5.4.7')

    with RelayLab(args.bin_dir) as lab:
        lab.peer.scenario = 'lost_final'
        with lab.connect('submissions', authenticate=True) as client:
            client.sendmail('alice@example.com', ['unknown@remote.test'], b'From: alice@example.com\r\n\r\nunknown\r\n')
        lab.await_states({'unknown@remote.test':'uncertain'})
        lab.stop()
        delivery = json.loads(lab.ctl('queue', 'list'))['id']
        lab.ctl('queue', 'retry', delivery, '--allow-duplicate')
        lab.peer.scenario = 'rcpt5'
        lab.restart()
        lab.await_states({'unknown@remote.test':'uncertain'})
        lab.stop()
        lab.ctl('queue', 'close-unknown', delivery, '--reason', 'Operator checked upstream; outcome remains unknown')
        shown = json.loads(lab.ctl('queue', 'show', delivery))['delivery']
        assert shown['state'] == 'uncertain' and shown['possibly_delivered'] and shown['closed_at_ms']
        history = [json.loads(line) for line in lab.ctl('queue', 'history', delivery).splitlines()]
        assert [r['action'] for r in history] == ['retry', 'close_unknown']
        with closing(sqlite3.connect(lab.base/'mail/meta.sqlite')) as db:
            assert db.execute('SELECT count(*) FROM notification').fetchone()[0] == 0
        lab.check_store(1)
    checks.append('L07: actual lost final result -> explicit retry -> later 550 still uncertain; auditable manual close preserves unknown outcome without a false failure DSN')
    report(args.output, checks=checks, pipeline_attempts=attempts, process_cuts=cuts, external_messages=0)


if __name__ == '__main__':
    main()
