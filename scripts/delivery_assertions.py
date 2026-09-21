"""Independent checks of the local-delivery prefix; no server code is imported."""
from email.parser import BytesHeaderParser
from email.policy import default
from email.utils import parsedate_to_datetime
import re


def delivery_content(blob, sender, protocol=None):
    """Validate the two generated fields, return retained client bytes and ID."""
    lines = blob.split(b'\r\n', 4)
    assert len(lines) == 5, 'missing delivery trace'
    assert lines[0] == b'Return-Path: <' + sender.encode('ascii') + b'>'
    assert lines[1].startswith(b'Received: from ')
    assert b'[127.0.0.1]' in lines[1] or b'[IPv6:::1]' in lines[1]
    assert lines[2].startswith(b'\tby mail.example.com with ')
    if protocol is not None:
        assert lines[2].endswith(b' with ' + protocol.encode('ascii'))
    match = re.fullmatch(rb'\tid ([0-9a-f]{32}); (.+)', lines[3])
    assert match, 'invalid trace ID/date'
    stamp = parsedate_to_datetime(match[2].decode('ascii'))
    assert stamp.utcoffset().total_seconds() == 0
    prefix = b'\r\n'.join(lines[:4]) + b'\r\n'
    parsed = BytesHeaderParser(policy=default).parsebytes(prefix + b'\r\n')
    assert not parsed.defects and list(parsed.keys()) == ['Return-Path', 'Received']
    assert len(prefix) + 2 <= 2048
    assert all(len(line) <= 998 for line in lines[:4])
    return lines[4], match[1].decode('ascii')
