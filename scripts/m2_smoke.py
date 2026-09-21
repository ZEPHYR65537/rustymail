#!/usr/bin/env python3
"""Independent TLS/AUTH/administration negatives against disposable loopback data."""
import argparse
import base64
import json
import os
from pathlib import Path
import re
import shutil
import smtplib
import socket
import sqlite3
import ssl
import subprocess
import tempfile
import time
from delivery_assertions import delivery_content

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', default='target/debug')
    parser.add_argument('--peer-denial-probe', action='store_true')
    parser.add_argument('--output', default='reports/local/m2-smoke.json')
    args = parser.parse_args()
    binaries = Path(args.bin_dir).resolve()
    suffix = '.exe' if os.name == 'nt' else ''
    hidden = {'creationflags': subprocess.CREATE_NO_WINDOW} if suffix else {}
    completed = []
    with tempfile.TemporaryDirectory(prefix='rustymail-m2-') as temporary:
        base = Path(temporary)
        certificates = base / 'certificates'
        subprocess.run([str(binaries / 'examples' / ('m2_certificates' + suffix)), str(certificates)], check=True, **hidden)
        with socket.socket() as reservation:
            reservation.bind(('127.0.0.1', 0))
            port = reservation.getsockname()[1]
        config = (ROOT / 'deploy/rustymail.lab.toml').read_text(encoding='utf-8')
        values = {'data_dir': str(base/'mail'), 'admin_socket': str(base/'admin'/'admin.sock'),
                  'certificate_file': str(base/'active.pem'), 'private_key_file': str(base/'active.key')}
        for key, value in values.items():
            config = re.sub(r'^' + key + r' = .*$', lambda _: key + ' = ' + json.dumps(value), config, flags=re.M)
        config = config.replace('submissions = "127.0.0.1:2465"', f'submissions = "127.0.0.1:{port}"')
        config = config.replace('disk_reserve_bytes = 2147483648', 'disk_reserve_bytes = 1')
        config = config.replace('disk_reserve_percent = 10', 'disk_reserve_percent = 1')
        config = config.replace('shutdown_grace_seconds = 60', 'shutdown_grace_seconds = 2')
        config = config.replace('handshake_timeout_seconds = 15', 'handshake_timeout_seconds = 1')
        config_path = base/'server.toml'
        config_path.write_text(config, encoding='utf-8')
        for source, target in [('server.pem','active.pem'),('server.key','active.key')]:
            shutil.copyfile(certificates/source,base/target)
            os.chmod(base/target,0o600)
        control = [str(binaries/('rustymailctl'+suffix)), '--config', str(config_path)]
        daemon = [str(binaries/('rustymaild'+suffix)), '--config', str(config_path), 'serve-lab-tls']
        socket_path = base/'admin'/'admin.sock'

        def ctl(*command, online=False, success=True):
            result = subprocess.run(control + (['--socket',str(socket_path)] if online else []) + list(command),
                                    capture_output=True, text=True, encoding='utf-8', timeout=90, **hidden)
            assert (result.returncode == 0) == success, (command,result.stderr)
            return json.loads(result.stdout) if success else None

        for name in ['alice','bob']:
            ctl('account','add',name+'@example.com')
        ctl('send-as','alice@example.com','alice@example.com')
        ctl('send-as','alice@example.com','bob@example.com',success=False)
        ctl('credential','create','alice@example.com','--label','no-stdout-leak',success=False)
        secrets = []
        def credential(login,label,scope='mail',online=False):
            path=base/(label+'.secret')
            result=ctl('credential','create',login,'--label',label,'--scope',scope,'--secret-output',str(path),online=online)
            assert 'application_password' not in result
            if os.name!='nt': assert path.stat().st_mode & 0o077 == 0
            token=path.read_text().strip();secrets.append(token)
            return token,result['selector']
        alice,selector=credential('alice@example.com','alice')
        readonly,_=credential('bob@example.com','readonly','read_only')
        before=ctl('credential','list','alice@example.com')
        assert len(before['credentials'])==1 and 'password_phc' not in json.dumps(before)
        completed.append('offline credential lifecycle and private one-time output')
        # A separate account keeps the existing authentication scenarios intact.
        # Seed pagination fixtures offline: hashing 100 unused passwords would
        # measure Argon2 instead of management framing.
        if os.name != 'nt':
            ctl('account', 'add', 'listing@example.com')
            with sqlite3.connect(base/'mail'/'meta.sqlite') as database:
                account = database.execute("SELECT id FROM account WHERE login=?", ('listing@example.com',)).fetchone()[0]
                phc = database.execute("SELECT password_phc FROM credential LIMIT 1").fetchone()[0]
                database.executemany(
                    "INSERT INTO credential(account_id,selector,label,password_phc,scope,created_at_ms) VALUES(?,?,?,?,?,?)",
                    [(account, f'{index+1000:032x}', '"\\'*40, phc, 'read_only', 0) for index in range(100)])
        context=ssl.create_default_context(cafile=str(certificates/'ca.pem'))
        log_path=base/'server.log'
        log=log_path.open('wb')
        process=None

        def start():
            nonlocal process
            process=subprocess.Popen(daemon,stdout=subprocess.DEVNULL,stderr=log,**hidden)
            deadline=time.monotonic()+45
            while time.monotonic()<deadline:
                if process.poll() is not None: raise AssertionError(log_path.read_text(encoding='utf-8'))
                try:
                    with socket.create_connection(('127.0.0.1',port),timeout=1): return
                except OSError: time.sleep(.05)
            raise AssertionError('TLS daemon did not start')

        def stop():
            nonlocal process
            if process and process.poll() is None:
                process.terminate()
                try: process.wait(timeout=15)
                except subprocess.TimeoutExpired: process.kill();process.wait(timeout=5)
            process=None

        def client(tls_context=context):
            connection=smtplib.SMTP_SSL('localhost',port,context=tls_context,timeout=30)
            assert connection.ehlo()[0]==250
            return connection

        def authenticated(token=alice,login='alice@example.com'):
            connection=client()
            assert connection.login(login,token)[0]==235
            return connection

        def rejected_tls(tls_context):
            try: connection=client(tls_context)
            except (ssl.SSLError,smtplib.SMTPServerDisconnected): return
            connection.close();raise AssertionError('invalid TLS peer accepted')

        try:
            start()
            rejected_tls(ssl.create_default_context())
            with socket.create_connection(('127.0.0.1',port)) as raw:
                try: context.wrap_socket(raw,server_hostname='wrong.example.test')
                except ssl.SSLCertVerificationError: pass
                else: raise AssertionError('wrong hostname accepted')
            with socket.create_connection(('127.0.0.1',port)) as stalled:
                stalled.settimeout(5)
                try: assert not stalled.recv(1)
                except ConnectionResetError: pass
            completed.append('unknown CA, wrong hostname and stalled TLS handshake rejected')
            c=client()
            try:
                assert c.mail('alice@example.com')[0]==530
                assert c.rcpt('alice@example.com')[0]==530
                for login,token in [('alice@example.com','wrong'),('unknown@example.com','wrong')]:
                    try: c.login(login,token)
                    except smtplib.SMTPAuthenticationError as error: assert error.smtp_code==535
                    else: raise AssertionError('invalid credentials accepted')
                forged=base64.b64encode(('bob@example.com\0alice@example.com\0'+alice).encode()).decode()
                assert c.docmd('AUTH','PLAIN '+forged)[0]==535
            finally: c.close()
            completed.append('AUTH required, wrong and unknown credentials return the same failure, authzid denied')
            c=authenticated()
            raw=b'From: Alice <alice@example.com>\r\nTo: bob@example.com\r\nSubject: M2\r\n\r\nPreserve TLS mail.\r\n'
            try:
                assert c.mail('bob@example.com')[0]==553
                assert c.mail('alice@example.com')[0]==250
                assert c.rcpt('outside@remote.test')[0]==550
                c.rset()
                assert not c.sendmail('alice@example.com',['bob@example.com'],raw)
                try: c.sendmail('alice@example.com',['bob@example.com'],raw.replace(b'Alice <alice@example.com>',b'bob@example.com'))
                except smtplib.SMTPDataError as error: assert error.smtp_code==550
                else: raise AssertionError('unauthorized From accepted')
            finally: c.close()
            for headers in [b'From: alice@example.com\r\nFrom: alice@example.com\r\n',
                            b'From: alice@example.com\r\nSender: bob@example.com\r\n',
                            b'From: alice@example.com\r\nResent-From: bob@example.com\r\n',
                            b'From: Alice\r\n <alice@example.com>\r\n']:
                c=authenticated()
                try:
                    try: c.sendmail('alice@example.com',['bob@example.com'],headers+b'\r\ninvalid\r\n')
                    except smtplib.SMTPDataError as error: assert error.smtp_code==550
                    else: raise AssertionError('ambiguous identity headers accepted')
                finally: c.close()
            c=authenticated(readonly,'bob@example.com')
            assert c.mail('bob@example.com')[0]==553
            c.close()
            completed.append('authorized local delivery; envelope, From, Sender, Resent, duplicate/folded identity negatives')
            if os.name!='nt':
                assert socket_path.stat().st_mode & 0o077 == 0
                assert socket_path.parent.stat().st_mode & 0o077 == 0
                assert ctl('status',online=True)['mode']=='lab_tls'
                page = ctl('credential', 'list', 'listing@example.com', '--limit', '100', online=True)['credentials']
                assert len(page) == 100 and all(item['label'] == '"\\'*40 for item in page)
                completed.append('100-entry credential page with maximum escaped labels crosses Unix management transport')
                online_secret,_=credential('bob@example.com','online',online=True)
                assert len(ctl('credential','list','bob@example.com',online=True)['credentials'])==2
                ctl('send-as','bob@example.com','alice@example.com',online=True,success=False)
                if args.peer_denial_probe:
                    try:
                        os.chmod(base,0o755);os.chmod(socket_path.parent,0o711);os.chmod(socket_path,0o666)
                        ctl('status',online=True,success=False)
                        probe="import socket,sys; s=socket.socket(socket.AF_UNIX); s.settimeout(5); s.connect(sys.argv[1]); data=s.recv(1); assert not data"
                        subprocess.run(['sudo','-n','-u','nobody','python3','-c',probe,str(socket_path)],check=True,capture_output=True,timeout=10)
                    finally:
                        os.chmod(socket_path,0o600);os.chmod(socket_path.parent,0o700);os.chmod(base,0o700)
                    completed.append('different Unix UID denied even with permissive test filesystem access')
                c=authenticated()
                assert c.mail('alice@example.com')[0]==250
                assert c.rcpt('bob@example.com')[0]==250
                assert c.docmd('DATA')[0]==354
                ctl('credential','revoke',selector,online=True)
                assert c.getreply()[0]==421;c.close()
                c=client()
                try: c.login('alice@example.com',alice)
                except smtplib.SMTPAuthenticationError as error: assert error.smtp_code==535
                else: raise AssertionError('revoked credential accepted')
                c.close()
                c=authenticated(online_secret,'bob@example.com')
                ctl('account','disable','bob@example.com',online=True)
                assert c.getreply()[0]==421;c.close()
                completed.append('online credential creation, in-flight DATA revocation and account disable')
                c=client();old_certificate=c.sock.getpeercert(binary_form=True)
                shutil.copyfile(certificates/'wrong.key',base/'active.key')
                ctl('reload-tls',online=True,success=False)
                test=client();assert test.sock.getpeercert(binary_form=True)==old_certificate;test.close()
                shutil.copyfile(certificates/'rotated.key',base/'active.key')
                shutil.copyfile(certificates/'rotated.pem',base/'active.pem')
                ctl('reload-tls',online=True)
                test=client();assert test.sock.getpeercert(binary_form=True)!=old_certificate;test.close()
                assert c.noop()[0]==250;c.close()
                completed.append('TLS reload preserves old config on failure and existing connections on success')
            stop()
            for cert in ['expired.pem','wrong-host.pem']:
                shutil.copyfile(certificates/cert,base/'active.pem')
                shutil.copyfile(certificates/'server.key',base/'active.key')
                start();rejected_tls(context);stop()
            completed.append('expired and wrong-name server certificates rejected by verified client')
            shutil.copyfile(certificates/'server.pem',base/'active.pem')
            config_path.write_text(config.replace('minimum_version = "1.2"','minimum_version = "1.3"'),encoding='utf-8')
            start()
            old_tls=ssl.create_default_context(cafile=str(certificates/'ca.pem'))
            old_tls.maximum_version=ssl.TLSVersion.TLSv1_2
            rejected_tls(old_tls)
            c=client();assert c.sock.version()=='TLSv1.3';c.close();stop()
            completed.append('minimum TLS version enforced')
            check=ctl('check-store');assert check['healthy'] and check['referenced_blobs']==1
            messages=subprocess.run(control+['mail','list','bob@example.com'],capture_output=True,text=True,check=True,**hidden)
            message=json.loads(messages.stdout)
            exported=base/'accepted.eml'
            ctl('mail','export','bob@example.com',message['message_id'],'--output',str(exported))
            content, _ = delivery_content(exported.read_bytes(), 'alice@example.com', 'ESMTPSA')
            assert content == raw
            completed.append('only authorized message persisted; final trace and retained client bytes verified after restart')
        finally:
            stop();log.close()
        logs=log_path.read_text(encoding='utf-8')
        for token in secrets:
            assert token not in logs and base64.b64encode(('\0alice@example.com\0'+token).encode()).decode() not in logs
        assert 'Preserve TLS mail.' not in logs and '$argon2id$' not in logs
        completed.append('logs contain no application passwords, AUTH payloads, PHC or mail body')
    report={'revision':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),
            'working_tree_dirty':bool(subprocess.check_output(['git','status','--porcelain'],text=True).strip()),
            'platform':os.name,'openssl':ssl.OPENSSL_VERSION,'checks':completed,'online_admin_tested':os.name!='nt',
            'different_uid_tested':os.name!='nt' and args.peer_denial_probe,'production_ready':False}
    output=Path(args.output);output.parent.mkdir(parents=True,exist_ok=True);output.write_text(json.dumps(report,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(report,indent=2))

if __name__=='__main__':
    main()
