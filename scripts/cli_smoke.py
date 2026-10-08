#!/usr/bin/env python3
"""Unified CLI via independent loopback peers; no external email or user store."""
import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import re
import signal
import smtplib
import socket
import subprocess
import tempfile
import time
from m42_smoke import Peer
from smtp_lab import ROOT, Lab, report

RAW = (b'From: alice@example.com\r\nTo: visible@example.com\r\nSubject: CLI test\r\n'
       b'Bcc: hidden@example.com\r\n\tcontinued\r\nResent-Bcc: hidden2@example.com\r\n'
       b'Return-Path: <fake@example.com>\r\n\r\n.leading\r\n\xff body\r\n')
EXPECTED = (b'From: alice@example.com\r\nTo: visible@example.com\r\nSubject: CLI test\r\n'
            b'\r\n.leading\r\n\xff body\r\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', default='target/debug')
    parser.add_argument('--output', default='reports/local/cli-smoke.json')
    args = parser.parse_args()
    binaries = Path(args.bin_dir).resolve()
    suffix = '.exe' if os.name == 'nt' else ''
    hidden = {'creationflags': subprocess.CREATE_NO_WINDOW} if suffix else {}
    executable = str(binaries / ('rustymail' + suffix))
    results = []
    with tempfile.TemporaryDirectory(prefix='rustymail-cli-') as directory:
        base = Path(directory)
        certificates = base / 'certificates'
        subprocess.run([str(binaries/'examples'/('m2_certificates'+suffix)), str(certificates)],
                       capture_output=True, check=True, timeout=30, **hidden)
        secret = base/'password.secret'
        secret.write_text('disposable-relay-password\n', encoding='ascii')
        secret.chmod(0o600)
        source = base/'message.eml'
        source.write_bytes(RAW)

        def configuration(peer, ca='certificates/ca.pem', username='relay-user', password='password.secret', max_bytes=26214400, header_bytes=65536):
            text = (f'sender = "alice@example.com"\n[smtp]\nhost = "localhost"\n'
                    f'port = {peer.port}\nsecurity = "{peer.mode}"\nehlo = "client.example.com"\n'
                    f'ca_file = {json.dumps(ca)}\nusername = {json.dumps(username)}\npassword_file = {json.dumps(password)}\n'
                    '[smtp.timeouts]\nconnect_timeout_seconds = 3\nhandshake_timeout_seconds = 3\n'
                    'banner_timeout_seconds = 2\ncommand_timeout_seconds = 2\ndata_command_timeout_seconds = 2\n'
                    'data_write_timeout_seconds = 2\nfinal_reply_timeout_seconds = 1\ndata_total_seconds = 6\n'
                    f'[limits]\nmessage_bytes = {max_bytes}\nheader_bytes = {header_bytes}\n')
            path = base/'client.toml'
            path.write_text(text, encoding='utf-8')
            return path

        def command(config, recipients, stdin=False, extra=()):
            command = [executable, 'send', '--config', str(config)]
            for recipient in recipients:
                command += ['--to', recipient]
            command += list(extra)
            if not stdin:
                command += ['--file', str(source)]
            return command

        def run(config, recipients=('one@remote.test',), stdin=False, expected=0, extra=()):
            result = subprocess.run(command(config, recipients, stdin, extra), input=source.read_bytes() if stdin else None,
                                    capture_output=True, timeout=45, cwd=base, **hidden)
            assert result.returncode == expected, (result.returncode, result.stdout, result.stderr)
            assert b'disposable-relay-password' not in result.stdout+result.stderr
            assert b'\xff body' not in result.stdout+result.stderr
            return [json.loads(line) for line in result.stdout.splitlines()]

        for mode, stdin in [('implicit', False), ('starttls', True)]:
            with closing(Peer(certificates, mode)) as peer:
                config = configuration(peer)
                rows = run(config, ['Case@remote.test', 'Case@REMOTE.test', 'case@remote.test'], stdin)
                assert [r['recipient'] for r in rows] == ['Case@remote.test', 'case@remote.test']
                assert all(r['status'] == 'accepted' for r in rows)
                assert len(peer.records) == 2
                assert all(r['body'] == EXPECTED and r['auth_encrypted'] for r in peer.records)
                results.append({'case': mode + ('-stdin' if stdin else '-file'), 'exit': 0, 'statuses':[r['status'] for r in rows]})

        for scenario, expected, status in [('rcpt4',1,'temporary_failure'), ('rcpt5',1,'permanent_failure'),
                ('data4',1,'temporary_failure'), ('data5',1,'permanent_failure'),
                ('lost_final',3,'uncertain'), ('final_timeout',3,'uncertain'),
                ('pre_body_disconnect',1,'not_submitted'), ('auth_fail',1,'not_submitted'),
                ('no_starttls',1,'not_submitted'), ('size',1,'not_submitted'), ('no_8bit',1,'not_submitted')]:
            with closing(Peer(certificates, 'starttls', scenario=scenario)) as peer:
                rows = run(configuration(peer), expected=expected)
                assert [r['status'] for r in rows] == [status]
                assert len(peer.records) == 1, 'automatic retry detected'
                results.append({'case':scenario,'exit':expected,'status':status})

        with closing(Peer(certificates, 'implicit', scenario='mixed')) as peer:
            rows = run(configuration(peer), ['Case@remote.test','fail@remote.test','defer@remote.test','lost@remote.test'], expected=3)
            assert [r['status'] for r in rows] == ['accepted','permanent_failure','temporary_failure','uncertain']
            assert len(peer.records) == 4
            results.append({'case':'mixed','exit':3,'statuses':[r['status'] for r in rows]})

        untrusted = base / 'untrusted-certificates'
        subprocess.run([str(binaries/'examples'/('m2_certificates'+suffix)), str(untrusted)],
                       capture_output=True, check=True, timeout=30, **hidden)
        for certificate, directory in [('wrong-host', certificates), ('expired', certificates), ('server', untrusted)]:
            with closing(Peer(directory, 'implicit', certificate=certificate)) as peer:
                rows = run(configuration(peer), expected=1)
                assert rows[0]['status'] == 'not_submitted'
                assert not any(r['auth_encrypted'] for r in peer.records)
                results.append({'case':'untrusted' if directory == untrusted else certificate,'exit':1,'credentials_sent':False})

        with closing(Peer(certificates, 'implicit')) as peer:
            config = configuration(peer, max_bytes=1024, header_bytes=64)
            for bad in [b'From: x\n\nbody\n', b'From: x\r\n\r\npartial', b'From: x\r\n\r\n\0\r\n', b'\r\n'+b'x\r\n'*342,
                        b'Bcc: '+b'x'*100+b'\r\n\r\n']:
                source.write_bytes(bad)
                assert run(config, expected=2) == []
            source.write_bytes(RAW)
            config = configuration(peer)
            config.write_text(config.read_text()+'\nunknown = "SECRET-SENTINEL"\n')
            result = subprocess.run(command(config, ['one@remote.test']), capture_output=True, timeout=10, **hidden)
            assert result.returncode == 2 and b'SECRET-SENTINEL' not in result.stderr
            assert not peer.records, 'bad input caused a network attempt'
            results.append({'case':'preflight-and-config','network_attempts':0})

        if os.name != 'nt':
            with closing(Peer(certificates, 'implicit', scenario='pause_final')) as peer:
                config = configuration(peer)
                process = subprocess.Popen(command(config,['one@remote.test','two@remote.test']), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    assert peer.body_seen.wait(10)
                    process.send_signal(signal.SIGINT)
                    stdout, stderr = process.communicate(timeout=10)
                    assert process.returncode == 3, (process.returncode, stderr)
                    assert [json.loads(line)['status'] for line in stdout.splitlines()] == ['uncertain','not_attempted']
                    assert len(peer.records) == 1
                finally:
                    if process.poll() is None:
                        process.kill(); process.wait(timeout=10)
                results.append({'case':'interrupt-after-body','exit':3})

        # Unified serve/admin/check remain usable; production is still gated.
        with socket.socket() as reservation:
            reservation.bind(('127.0.0.1',0)); port = reservation.getsockname()[1]
        text=(ROOT/'deploy/rustymail.lab.toml').read_text(encoding='utf-8')
        for key,value in {'data_dir':str(base/'store'),'smtp':f'127.0.0.1:{port}','disk_reserve_bytes':1,'disk_reserve_percent':1}.items():
            text=re.sub(r'^'+key+r' = .*$',lambda _:f'{key} = {json.dumps(value)}',text,count=1,flags=re.M)
        server_config=base/'server.toml'; server_config.write_text(text,encoding='utf-8')
        for role, tail, code in [('check',[],0),('serve',[],1),('admin',['account','add','alice@example.com'],0)]:
            result=subprocess.run([executable,role,'--config',str(server_config),*tail],capture_output=True,timeout=30,**hidden)
            assert result.returncode == code, result.stderr
            if role != 'admin': assert not (base/'store').exists()
        process=subprocess.Popen([executable,'serve','--config',str(server_config),'--mode','lab'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,**hidden)
        try:
            deadline=time.monotonic()+15
            while True:
                try:
                    with smtplib.SMTP('127.0.0.1',port,timeout=3) as smtp:
                        smtp.sendmail('sender@remote.test',['alice@example.com'],b'From: sender@remote.test\r\n\r\nbody\r\n')
                    break
                except OSError:
                    assert process.poll() is None and time.monotonic()<deadline
                    time.sleep(.05)
        finally:
            process.kill(); process.wait(timeout=10)
        listing=subprocess.run([executable,'admin','--config',str(server_config),'mail','list','alice@example.com'],capture_output=True,timeout=20,**hidden)
        assert listing.returncode == 0 and len(listing.stdout.splitlines()) == 1
        results.append({'case':'unified-serve-admin-check','accepted_local_messages':1})

    # A real client invocation interoperates with the existing server credentials.
    with Lab(args.bin_dir) as lab:
        config=lab.base/'client.toml'
        config.write_text(f'sender="alice@example.com"\n[smtp]\nhost="localhost"\nport={lab.ports["submissions"]}\nsecurity="implicit"\nehlo="client.test"\nca_file="certificates/ca.pem"\nusername="alice@example.com"\npassword_file="credential.secret"\n')
        result=subprocess.run([executable,'send','--config',str(config),'--to','bob@example.com'],input=b'From: alice@example.com\r\n\r\nHello from unified CLI\r\n',capture_output=True,timeout=30,**hidden)
        assert result.returncode == 0, result.stderr
        assert json.loads(result.stdout)['status'] == 'accepted'
        lab.check_store(1)
        results.append({'case':'client-to-rustymail-tls','status':'accepted'})
    report(args.output,checks=results,external_messages=0)


if __name__ == '__main__':
    main()
