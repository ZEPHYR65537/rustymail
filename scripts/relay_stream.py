#!/usr/bin/env python3
"""Linux one-attempt TLS streaming sample, independent of queue throughput."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from m42_smoke import Peer, configuration
from smtp_lab import report
from smtp_pressure import snapshot


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', default='target/release')
    parser.add_argument('--output', default='reports/local/relay-stream.json')
    args = parser.parse_args()
    assert os.name == 'posix' and Path('/proc/self/status').exists(), 'requires Linux /proc'
    binaries = Path(args.bin_dir).resolve()
    samples = []
    with tempfile.TemporaryDirectory(prefix='rustymail-stream-') as temporary:
        base = Path(temporary)
        certificates = base/'certificates'
        subprocess.run([str(binaries/'examples/m2_certificates'),str(certificates)],check=True,capture_output=True)
        for target_size in [1024*1024,25*1024*1024]:
            source = base/'source.eml'
            digest = hashlib.sha256()
            with source.open('wb') as output:
                header = b'From: sender@example.test\r\nSubject: synthetic stream\r\n\r\n'
                output.write(header)
                digest.update(header)
                line = b'.'+b'x'*997+b'\r\n'
                for _ in range((target_size-len(header))//len(line)):
                    output.write(line)
                    digest.update(line)
            peer = Peer(certificates,'implicit')
            process = None
            try:
                config = configuration(base,peer,certificates)
                start = time.monotonic()
                process = subprocess.Popen([str(binaries/'examples/m42_attempt'),str(config),str(source),'target@remote.test','tls'],
                                           stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                observations = []
                while process.poll() is None:
                    assert time.monotonic()-start < 60, 'stream deadline'
                    try:
                        sample = snapshot(process.pid)
                        if sample and 'VmHWM' in sample:
                            observations.append(sample)
                    except (OSError,ProcessLookupError):
                        pass  # /proc entries may disappear at normal exit.
                    time.sleep(.005)
                stdout,stderr = process.communicate(timeout=5)
                assert process.returncode == 0, stderr.decode(errors='replace')
                assert json.loads(stdout) == {'result':{'Delivered':250},'body_marked':True}
                assert observations and len(peer.records)==1
                assert hashlib.sha256(peer.records[0]['body']).digest()==digest.digest()
                samples.append({'stored_bytes':source.stat().st_size,'seconds':round(time.monotonic()-start,4),
                                'samples':len(observations),'rss_kib':max(s['VmRSS'] for s in observations),
                                'hwm_kib':max(s['VmHWM'] for s in observations),'fds':max(s['fds'] for s in observations)})
            finally:
                if process and process.poll() is None:
                    process.kill()
                    process.communicate(timeout=5)
                peer.close()
    assert samples[1]['hwm_kib'] < 64*1024
    assert samples[1]['hwm_kib']-samples[0]['hwm_kib'] < 8*1024
    report(args.output,samples=samples,sample_interval_ms=5,stream_buffer_bytes=16384,
           scope='one Rust TLS attempt process; excludes Python peer, storage, queue and Argon2; sampled peaks are lower bounds',
           external_messages=0)


if __name__=='__main__':
    main()
