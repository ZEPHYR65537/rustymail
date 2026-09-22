#!/usr/bin/env python3
"""Independent SMTP peer: real TCP first, then verified TLS and durable relay."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import base64
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import ssl
import stat
import subprocess
import tempfile
import threading
import time
from smtp_lab import ROOT, Lab, report


class Peer:
    def __init__(self, certificates=None, tls='plain', certificate='server', scenario='ok'):
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.listener.listen(16)
        self.listener.settimeout(.1)
        self.port = self.listener.getsockname()[1]
        self.mode, self.scenario = tls, scenario
        self.records, self.errors = [], []
        self.stop = threading.Event()
        self.body_seen = threading.Event()
        self.release_final = threading.Event()
        self.pool = ThreadPoolExecutor(max_workers=16)
        self.context = None
        if certificates:
            self.context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            self.context.load_cert_chain(certificates/(certificate+'.pem'), certificates/'server.key')
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stop.is_set():
            try:
                stream, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            self.pool.submit(self.connection, stream)

    def connection(self, stream):
        try:
            with stream:
                stream.settimeout(5)
                stream.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                self.session(stream)
        except (OSError, ssl.SSLError):
            pass  # Deliberately broken TLS/aborted attempts are expected.
        except BaseException as error:
            self.errors.append(repr(error))

    def session(self, stream):
        secured = self.mode == 'implicit'
        if secured:
            stream = self.context.wrap_socket(stream, server_side=True)
        record = {'commands':[], 'body':None, 'auth_encrypted':None}
        scenario = self.scenario
        self.records.append(record)
        reader = stream.makefile('rb')
        try:
            if self.scenario == 'banner_timeout':
                self.stop.wait(2)
                return
            stream.sendall(b'220 peer.test ready\r\n')
            while not self.stop.is_set():
                line = reader.readline(2049)
                if not line:
                    return
                if line.upper().startswith(b'AUTH PLAIN '):
                    record['auth_encrypted'] = secured
                    expected = b'\0relay-user\0disposable-relay-password'
                    assert base64.b64decode(line.split()[2], validate=True) == expected
                    if self.scenario == 'auth_challenge':
                        stream.sendall(b'334 \r\n')
                        assert base64.b64decode(reader.readline(2049).strip(), validate=True) == expected
                    stream.sendall(b'535 auth rejected\r\n' if self.scenario == 'auth_fail' else b'235 authenticated\r\n')
                    continue  # Never record the encoded credential.
                command = line.decode('ascii').strip()
                record['commands'].append(command)
                if command.startswith('EHLO '):
                    if self.scenario == 'mixed_reply':
                        stream.sendall(b'250-peer\r\n550 invalid continuation\r\n')
                        return
                    if self.scenario == 'long_reply':
                        stream.sendall(b'250 '+b'x'*509+b'\r\n')
                        return
                    if self.scenario == 'many_reply':
                        stream.sendall(b'250-peer\r\n'+b'250-X\r\n'*64+b'250 end\r\n')
                        return
                    extensions = [b'250-peer.test']
                    if self.mode == 'starttls' and not secured and self.scenario != 'no_starttls':
                        extensions.append(b'250-STARTTLS')
                    if self.scenario != 'no_8bit' and not (self.scenario == 'cap_reset' and secured):
                        extensions.append(b'250-8BITMIME')
                    if secured:
                        extensions.append(b'250-AUTH PLAIN')
                    extensions.append(b'250 SIZE '+(b'1' if self.scenario == 'size' else b'30000000'))
                    reply = b'\r\n'.join(extensions)+b'\r\n'
                    if self.scenario == 'fragmented_reply':
                        for byte in reply:
                            stream.sendall(bytes([byte]))
                    else:
                        stream.sendall(reply)
                elif command == 'STARTTLS':
                    assert not secured
                    stream.sendall(b'220 upgrade\r\n')
                    reader.close()
                    stream = self.context.wrap_socket(stream, server_side=True)
                    reader = stream.makefile('rb')
                    secured = True
                elif command.startswith('MAIL FROM:'):
                    stream.sendall(b'250 sender\r\n')
                elif command.startswith('RCPT TO:'):
                    record['recipient'] = command.removeprefix('RCPT TO:<').removesuffix('>')
                    if self.scenario == 'mixed':
                        scenario = {'defer':'rcpt4', 'fail':'rcpt5', 'lost':'lost_final'}.get(record['recipient'].split('@')[0], 'ok')
                    code = 450 if scenario == 'rcpt4' else 550 if scenario == 'rcpt5' else 250
                    stream.sendall(f'{code} recipient\r\n'.encode())
                elif command == 'DATA':
                    if scenario == 'pre_body_disconnect':
                        return
                    stream.sendall(b'354 body\r\n')
                    body = bytearray()
                    while True:
                        chunk = reader.readline(1002)
                        if not chunk:
                            return
                        assert chunk.endswith(b'\r\n') and len(chunk) <= 1001
                        if chunk == b'.\r\n':
                            break
                        body.extend(chunk[1:] if chunk.startswith(b'..') else chunk)
                    record['body'] = bytes(body)
                    self.body_seen.set()
                    if scenario == 'pause_final':
                        assert self.release_final.wait(30), 'missing crash-test release'
                    if scenario == 'lost_final':
                        return
                    if scenario == 'final_timeout':
                        self.stop.wait(2)
                        return
                    code = 450 if scenario == 'data4' else 550 if scenario == 'data5' else 250
                    stream.sendall(f'{code} final\r\n'.encode())
                else:
                    raise AssertionError(command)
        finally:
            reader.close()
            stream.close()

    def close(self):
        self.stop.set()
        self.release_final.set()
        self.listener.close()
        self.thread.join(timeout=8)
        self.pool.shutdown(wait=True, cancel_futures=True)
        assert not self.thread.is_alive() and not self.errors, self.errors


def configuration(base, peer, certificates, authenticate=False):
    values = {'data_dir':str(base/'mail'), 'host':'localhost', 'port':peer.port,
              'username':'relay-user' if authenticate else '', 'password_file':str(base/'relay.secret'),
              'disk_reserve_bytes':1, 'disk_reserve_percent':1, 'connect_timeout_seconds':5,
              'banner_timeout_seconds':1, 'command_timeout_seconds':2, 'data_command_timeout_seconds':2,
              'data_write_timeout_seconds':2, 'final_reply_timeout_seconds':1}
    text = (ROOT/'deploy/rustymail.lab.toml').read_text(encoding='utf-8')
    for key, value in values.items():
        text, count = re.subn(r'^'+key+r' = .*$', lambda _:f'{key} = {json.dumps(value)}', text, count=1, flags=re.M)
        assert count == 1, key
    text = text.replace('tls = "implicit"', 'tls = '+json.dumps('starttls' if peer.mode == 'starttls' else 'implicit'))
    text = text.replace('[relay]', '[relay]\nca_file = '+json.dumps(str(certificates/'ca.pem')))
    config = base/'attempt.toml'
    config.write_text(text, encoding='utf-8')
    return config


class RelayLab(Lab):
    command = 'serve-lab-relay'

    def __init__(self, bin_dir):
        super().__init__(bin_dir, {'recipients_per_message':8})
        self.peer = None

    def configure(self, text, certificates):
        self.peer = Peer(certificates, 'starttls', scenario='mixed')
        secret = self.base/'relay.secret'
        secret.write_text('disposable-relay-password\n', encoding='ascii')
        secret.chmod(0o600)
        values = {'host':'localhost','port':self.peer.port,'username':'relay-user','password_file':str(secret),
                  'connect_timeout_seconds':5,'banner_timeout_seconds':2,'command_timeout_seconds':2,
                  'data_command_timeout_seconds':2,'data_write_timeout_seconds':2,'final_reply_timeout_seconds':2}
        for key, value in values.items():
            text, count = re.subn(r'^'+key+r' = .*$',lambda _:f'{key} = {json.dumps(value)}',text,count=1,flags=re.M)
            assert count == 1
        return text.replace('mode = "disabled"','mode = "relay"').replace('tls = "implicit"','tls = "starttls"').replace(
            '[relay]', '[relay]\nca_file = '+json.dumps(str(certificates/'ca.pem')))

    def rows(self):
        with closing(sqlite3.connect(self.base/'mail/meta.sqlite')) as db:
            return db.execute("SELECT recipient,state,attempts FROM delivery WHERE route='relay' ORDER BY recipient").fetchall()

    def await_states(self, expected):
        deadline = time.monotonic()+35
        while True:
            rows = self.rows()
            if {r[0]:r[1] for r in rows} == expected:
                return rows
            assert time.monotonic() < deadline, (rows,self.log_path.read_text(encoding='utf-8'))
            time.sleep(.1)

    def restart(self):
        assert self.process.poll() is not None
        self.process = subprocess.Popen([str(self.binaries/('rustymaild'+self.suffix)), '--config',str(self.config),self.command],
                                        stdout=subprocess.DEVNULL,stderr=self.log,**self.hidden)
        deadline = time.monotonic()+30
        while True:
            try:
                with self.connect() as client:
                    assert client.noop()[0] == 250
                return
            except OSError:
                assert self.process.poll() is None and time.monotonic() < deadline, self.log_path.read_text(encoding='utf-8')
                time.sleep(.05)

    def __exit__(self,*args):
        try:
            super().__exit__(*args)
        finally:
            if self.peer:
                self.peer.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', default='target/debug')
    parser.add_argument('--output', default='reports/local/m42-smoke.json')
    args = parser.parse_args()
    binaries = Path(args.bin_dir).resolve()
    suffix = '.exe' if os.name == 'nt' else ''
    hidden = {'creationflags':subprocess.CREATE_NO_WINDOW} if suffix else {}
    checks, attempts = [], []
    with tempfile.TemporaryDirectory(prefix='rustymail-relay-') as temporary:
        base = Path(temporary)
        certificates = base/'certificates'
        subprocess.run([str(binaries/'examples'/('m2_certificates'+suffix)), str(certificates)], check=True, capture_output=True, **hidden)
        untrusted = base/'untrusted'
        subprocess.run([str(binaries/'examples'/('m2_certificates'+suffix)), str(untrusted)], check=True, capture_output=True, **hidden)
        secret = base/'relay.secret'
        secret.write_text('disposable-relay-password\n', encoding='ascii')
        secret.chmod(0o600)
        source = base/'sample.eml'
        raw = b'From: sender@example.com\r\nSubject: single attempt\r\n\r\n.dot\r\n\xff\r\n'
        source.write_bytes(raw)
        cases = [('plain','server',s,result,marked) for s,result,marked in [
            ('ok',{'Delivered':250},True),('fragmented_reply',{'Delivered':250},True),
            ('rcpt4',{'Temporary':450},False),('rcpt5',{'Permanent':550},False),
            ('data4',{'Temporary':450},True),('data5',{'Permanent':550},True),('pre_body_disconnect','ConnectionLost',False),
            ('lost_final','ConnectionLost',True),('final_timeout','ConnectionLost',True),
            ('mixed_reply','ConnectionLost',False),('long_reply','ConnectionLost',False),('many_reply','ConnectionLost',False),
            ('banner_timeout','ConnectionLost',False),('no_8bit','Hold',False),('size','Hold',False)]]
        cases += [('implicit','server','ok',{'Delivered':250},True),('starttls','server','ok',{'Delivered':250},True),
                  ('implicit','wrong-host','ok','Deferred',False),('implicit','expired','ok','Deferred',False),
                  ('starttls','server','no_starttls','Deferred',False),('starttls','server','cap_reset','Hold',False),
                  ('implicit','server','auth_fail','Deferred',False),
                  ('starttls','server','auth_challenge',{'Delivered':250},True),
                  ('implicit','untrusted','ok','Deferred',False)]
        for mode, certificate, scenario, expected, marked in cases:
            peer = Peer(untrusted if certificate=='untrusted' else certificates, mode,
                        'server' if certificate=='untrusted' else certificate, scenario)
            try:
                config = configuration(base, peer, certificates, authenticate=mode!='plain')
                start = time.monotonic()
                result = subprocess.run([str(binaries/'examples'/('m42_attempt'+suffix)),str(config),str(source),
                                         'target@remote.test','plain' if mode=='plain' else 'tls'],
                                        check=True, capture_output=True, text=True, encoding='utf-8', timeout=15, **hidden)
                observed = json.loads(result.stdout)
                assert observed == {'result':expected, 'body_marked':marked}, (mode,scenario,observed,peer.records,peer.errors)
                assert 'disposable-relay-password' not in result.stderr
                if isinstance(expected,dict) and 'Delivered' in expected:
                    assert peer.records[-1]['body'] == raw
                if mode!='plain' and peer.records and peer.records[-1]['auth_encrypted'] is not None:
                    assert peer.records[-1]['auth_encrypted']
                attempts.append({'mode':mode,'certificate':certificate,'scenario':scenario,**observed,
                                 'seconds':round(time.monotonic()-start,3)})
            finally:
                peer.close()
        checks.append('R01: real TCP; per-recipient and final DATA classifications; lost results; strict bounded replies and deadlines')
        checks.append('R02: implicit TLS/STARTTLS; post-upgrade capabilities; verified CA/hostname/expiry; encrypted AUTH including challenge and no plaintext fallback')
    with RelayLab(args.bin_dir) as lab:
        # Public receiver, forged local envelope and unauthenticated submission
        # still cannot acquire remote delivery responsibility.
        with lab.connect() as client:
            assert client.mail('alice@example.com')[0] == 250
            assert client.rcpt('outside@remote.test')[0] == 550
        with lab.connect('submission') as client:
            assert client.mail('alice@example.com')[0] == 530
        with lab.connect('submissions',authenticate=True) as client:
            assert client.mail('forged@example.com')[0] == 553
            recipients = ['bob@example.com','Case@remote.test','case@REMOTE.TEST','defer@remote.test','fail@remote.test','lost@remote.test']
            raw = b'From: alice@example.com\r\nBcc: secret@remote.test\r\n\tcontinued\r\nSubject: mixed\r\n\r\n.dot\r\n\xff\r\n'
            assert client.sendmail('alice@example.com',recipients,raw,mail_options=['BODY=8BITMIME']) == {}
        expected = {'Case@remote.test':'delivered','case@remote.test':'delivered','defer@remote.test':'deferred',
                    'fail@remote.test':'failed','lost@remote.test':'uncertain'}
        initial = lab.await_states(expected)
        assert all(row[2] == 1 for row in initial)
        lab.stop()
        # Inspect final stored bytes independently from the network peer.
        with closing(sqlite3.connect(lab.base/'mail/meta.sqlite')) as db:
            message_id,blob_id = db.execute('SELECT id,blob_id FROM message').fetchone()
            assert db.execute('SELECT count(*) FROM mailbox_message').fetchone()[0] == 1
            assert db.execute('SELECT count(*) FROM blob').fetchone()[0] == 1
            assert db.execute('SELECT used_bytes FROM account WHERE login=?',('bob@example.com',)).fetchone()[0] > 0
        stored = (lab.base/'mail/blobs'/(blob_id+'.eml')).read_bytes()
        outbound = stored.split(b'\r\n',1)[1]
        assert stored.startswith(b'Return-Path: <alice@example.com>\r\nReceived: ')
        assert b'Bcc:' not in stored and b'continued' not in stored
        transmitted = [r for r in lab.peer.records if r['body'] is not None]
        assert len(transmitted) == 3 and all(r['body'] == outbound for r in transmitted)
        assert all(r['auth_encrypted'] for r in transmitted)
        assert lab.check_store(1)['queue_mismatches'] == 0
        count = len(lab.peer.records)
        lab.restart()
        time.sleep(2.5)
        assert lab.rows() == initial and len(lab.peer.records) == count
        lab.stop()
        rows = [json.loads(line) for line in lab.ctl('queue','list').splitlines()]
        retry_id = next(r['id'] for r in rows if r['recipient']=='defer@remote.test')
        lab.ctl('queue','retry',retry_id)
        lab.peer.scenario = 'ok'
        lab.restart()
        expected['defer@remote.test'] = 'delivered'
        retried = lab.await_states(expected)
        assert next(r[2] for r in retried if r[0]=='defer@remote.test') == 2
        lab.check_store(1)
        assert 'disposable-relay-password' not in lab.log_path.read_text(encoding='utf-8')
        checks.append('R03: authenticated mixed submission; atomic local/remote responsibility; one blob; Bcc privacy; verified Return-Path projection')
        checks.append('R04: remote case preservation; durable delivered/deferred/failed/uncertain; restart does not resend; explicit retry only sends the selected recipient')
    with RelayLab(args.bin_dir) as lab:
        lab.peer.scenario = 'pause_final'
        with lab.connect('submissions',authenticate=True) as client:
            assert client.sendmail('alice@example.com',['crash@remote.test'],b'From: alice@example.com\r\n\r\ncrash boundary\r\n') == {}
        assert lab.peer.body_seen.wait(15)
        with closing(sqlite3.connect(lab.base/'mail/meta.sqlite')) as db:
            assert db.execute('SELECT phase FROM queue_lease').fetchall() == [('body',)]
        admin_path = lab.base/'admin/admin.sock'
        admin_before = admin_path.lstat() if os.name == 'posix' else None
        lab.process.kill()
        lab.process.wait(timeout=10)
        # Model explicit operator recovery only for this known, reaped child
        # and its unchanged socket inside our private temporary directory.
        if admin_before:
            current = admin_path.lstat()
            assert stat.S_ISSOCK(current.st_mode) and current.st_uid == os.getuid()
            assert (current.st_dev,current.st_ino) == (admin_before.st_dev,admin_before.st_ino)
            assert admin_path.parent.stat().st_mode & 0o077 == 0
            admin_path.unlink()
        lab.peer.release_final.set()
        lab.peer.scenario = 'ok'
        lab.restart()
        crash_recovery = lab.await_states({'crash@remote.test':'uncertain'})
        assert crash_recovery == [('crash@remote.test','uncertain',1)]
        time.sleep(2)
        assert len([r for r in lab.peer.records if r['body'] is not None]) == 1
        lab.check_store(1)
        checks.append('R05: kill real relay after peer receives DATA; supervised restart (remove known stale Unix admin socket after reaping child) preserves uncertain without resending')
    report(args.output, checks=checks, attempts=attempts, queue_initial=initial,queue_after_retry=retried,
           crash_recovery=crash_recovery,stale_admin_socket_removed=admin_before is not None,external_messages=0)


if __name__ == '__main__':
    main()
