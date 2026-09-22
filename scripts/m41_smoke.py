#!/usr/bin/env python3
"""Offline queue CLI and real process-kill boundaries; never sends external mail."""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import sqlite3
import subprocess
import tempfile
import threading
from smtp_lab import ROOT, report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', default='target/debug')
    parser.add_argument('--output', default='reports/local/m41-smoke.json')
    args = parser.parse_args()
    binaries = Path(args.bin_dir).resolve()
    suffix = '.exe' if os.name == 'nt' else ''
    hidden = {'creationflags': subprocess.CREATE_NO_WINDOW} if suffix else {}
    control = binaries/('rustymailctl'+suffix)
    probe = binaries/'examples'/('m41_probe'+suffix)
    checks, cuts = [], []

    def run(command, success=True):
        result = subprocess.run([str(x) for x in command], capture_output=True,
                                text=True, encoding='utf-8', timeout=60, **hidden)
        assert (result.returncode == 0) == success, (result.returncode, result.stderr)
        return result.stdout

    with tempfile.TemporaryDirectory(prefix='rustymail-queue-') as temporary:
        base = Path(temporary)
        root = base/'mail'
        config = base/'server.toml'
        text = (ROOT/'deploy/rustymail.lab.toml').read_text(encoding='utf-8')
        for key, value in {'data_dir':str(root), 'disk_reserve_bytes':1, 'disk_reserve_percent':1}.items():
            text, count = re.subn(r'^'+key+r' = .*$', lambda _: f'{key} = {json.dumps(value)}',
                                 text, count=1, flags=re.M)
            assert count == 1
        config.write_text(text, encoding='utf-8')

        def ctl(*values, success=True):
            return run([control, '--config', config, *values], success)

        ctl('account', 'add', 'alice@example.com')
        source = base/'fixture.eml'
        raw = b'From: sender@example.com\r\nSubject: queue lab\r\n\r\nimmutable\r\n'
        source.write_bytes(raw)
        operation = '22222222222222222222222222222222'
        command = ['queue', 'import-lab', '--source', source, '--operation-id', operation,
                   '--recipient', 'Case@remote.test', '--recipient', 'case@REMOTE.TEST',
                   '--recipient', 'Case@remote.test', '--recipient', 'third@other.test']
        accepted = json.loads(ctl(*command))
        replay = json.loads(ctl(*command))
        assert not accepted['already_committed'] and replay['already_committed']
        assert accepted['message_id'] == replay['message_id'] and not replay['transmitted']
        for extra in [['--sender', 'sender@example.com'], ['--body', '8bitmime'],
                      ['--max-age-seconds', '3600'], ['--recipient', 'different@remote.test']]:
            ctl(*(command+extra), success=False)
        source.write_bytes(raw+b'changed\r\n')
        ctl(*command, success=False)
        source.write_bytes(raw)
        ctl('queue', 'import-lab', '--source', source, '--operation-id', '3'*32,
            '--recipient', 'alice@example.com', success=False)
        first = [json.loads(line) for line in ctl('queue', 'list', '--limit', '2').splitlines()]
        second = [json.loads(line) for line in ctl('queue', 'list', '--limit', '2',
                                                '--after-id', first[-1]['id']).splitlines()]
        tasks = first+second
        assert len(first) == 2 and len(second) == 1 and len({t['id'] for t in tasks}) == 3
        assert {t['recipient'] for t in tasks} == {'Case@remote.test', 'case@remote.test', 'third@other.test'}
        assert all(t['state'] == 'pending' and t['attempts'] == 0 and 'lease_token' not in t for t in tasks)
        ctl('queue', 'list', '--limit', '129', success=False)
        checks.append('Q01: atomic shared-blob import; external case; strict idempotence; bounded keyset list')

        task = tasks[0]['id']
        ctl('queue', 'hold', task)
        ctl('queue', 'retry', task, success=False)
        ctl('queue', 'retry', task, '--allow-duplicate')
        assert json.loads(ctl('queue', 'recover'))['recovered'] == 0
        result = json.loads(ctl('check-store'))
        assert result['queue_mismatches'] == 0
        # This experiment owns an idle, disposable store; inspect independently.
        with closing(sqlite3.connect(root/'meta.sqlite')) as connection:
            assert connection.execute('PRAGMA user_version').fetchone()[0] == 3
            assert connection.execute('PRAGMA foreign_key_check').fetchall() == []
            assert connection.execute('SELECT count(*) FROM message').fetchone()[0] == 1
            assert connection.execute('SELECT count(*) FROM mailbox_message').fetchone()[0] == 0
            assert connection.execute('SELECT reverse_path FROM message').fetchone()[0] == ''
            blob_id, size, digest = connection.execute('SELECT id,size_bytes,sha256 FROM blob').fetchone()
        blob = root/'blobs'/(blob_id+'.eml')
        assert blob.read_bytes() == raw and size == len(raw) and digest == hashlib.sha256(raw).hexdigest()
        ctl('gc', '--apply', '--min-age-seconds', '0')
        assert blob.read_bytes() == raw
        checks.append('Q02: explicit hold/retry consent; null sender; integrity; GC retains referenced queue body')

        expected = {
            ('enqueue', 'before'): (None, None, None),
            ('enqueue', 'after'): ('pending', None, 'pending'),
            ('claim', 'before'): ('pending', None, 'pending'),
            ('claim', 'after'): ('leased', 'ready', 'deferred'),
            ('body', 'before'): ('leased', 'ready', 'deferred'),
            ('body', 'after'): ('leased', 'body', 'uncertain'),
            ('finish', 'before'): ('leased', 'body', 'uncertain'),
            ('finish', 'after'): ('delivered', None, 'delivered'),
        }
        for (action, boundary), (before_state, before_phase, recovered_state) in expected.items():
            cut_root = base/(action+'-'+boundary)
            if action != 'enqueue':
                run([probe, cut_root, 'seed', 'none'])
            lines = queue.Queue()
            # Redirect stderr to a file, so a failed child cannot fill a pipe.
            with (base/(action+'-'+boundary+'.log')).open('wb') as errors:
                process = subprocess.Popen([str(probe), str(cut_root), action, boundary],
                                           stdout=subprocess.PIPE, stderr=errors, text=True,
                                           encoding='utf-8', **hidden)

                def read_marker():
                    for line in process.stdout:
                        lines.put(line.strip())

                reader = threading.Thread(target=read_marker, daemon=True)
                reader.start()
                try:
                    assert lines.get(timeout=30) == 'RUSTYMAIL_QUEUE_CUT'
                    process.kill()  # TerminateProcess on Windows, SIGKILL on Linux.
                    process.wait(timeout=10)
                finally:
                    if process.poll() is None:
                        process.kill()
                        process.wait(timeout=10)
                    reader.join(timeout=5)
                    assert not reader.is_alive()
                    process.stdout.close()
            with closing(sqlite3.connect(cut_root/'meta.sqlite')) as connection:
                row = connection.execute('SELECT d.state,q.phase FROM delivery d LEFT JOIN queue_lease q ON q.delivery_id=d.id').fetchone()
                assert row == ((before_state, before_phase) if before_state else None), (action, boundary, row)
            recovered = json.loads(run([probe, cut_root, 'recover', 'none']))
            rows = recovered['queue']
            assert ([r['state'] for r in rows] == [recovered_state]) if recovered_state else not rows
            assert recovered['integrity']['queue_mismatches'] == 0
            cuts.append({'action':action, 'boundary':boundary, 'persisted_state':before_state,
                         'persisted_phase':before_phase, 'recovered_state':recovered_state,
                         'integrity':recovered['integrity']})
        checks.append('Q03: eight actual process kills around enqueue/claim/body/result commits; durable recovery states')
    report(args.output, checks=checks, process_cuts=cuts, external_messages=0)


if __name__ == '__main__':
    main()
