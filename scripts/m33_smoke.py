#!/usr/bin/env python3
"""M3.3 executable input contract. Independent smtplib plus exact wire bytes."""
import argparse
import smtplib
import time
from delivery_assertions import delivery_content
from smtp_lab import Lab, report


def closed(client):
    try:
        client.noop()
    except (smtplib.SMTPServerDisconnected, OSError):
        return
    raise AssertionError('connection survived fatal input')


def envelope(client, options=()):
    assert client.mail('sender@remote.test', options=options)[0] == 250
    assert client.rcpt('alice@example.com')[0] == 250
    client.putcmd('DATA')
    assert client.getreply()[0] == 354


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', default='target/debug')
    parser.add_argument('--output', default='reports/local/m33-smoke.json')
    args = parser.parse_args()
    checks, expected = [], {}
    with Lab(args.bin_dir) as lab:
        # Advertised extensions match transport/authorization state exactly.
        for role, tls in [('smtp', False), ('smtp', True), ('submission', False), ('submission', True), ('submissions', True)]:
            with lab.connect(role, starttls=tls) as client:
                if role == 'smtp' and tls:
                    assert client.starttls(context=lab.context)[0] == 220
                    assert client.ehlo()[0] == 250
                capabilities = {'size', '8bitmime', 'enhancedstatuscodes'}
                if not tls:
                    capabilities.add('starttls')
                elif role != 'smtp':
                    capabilities.add('auth')
                assert set(client.esmtp_features) == capabilities, client.esmtp_features
                assert client.esmtp_features['size'] == '65536'
                if tls and role != 'smtp':
                    assert client.esmtp_features['auth'].strip() == 'PLAIN'
                if role != 'smtp':
                    assert client.mail('alice@example.com')[0] == 530
                else:
                    assert client.docmd('AUTH', 'PLAIN')[0] == 502
        with lab.connect('submissions', authenticate=True) as client:
            assert client.ehlo()[0] == 250
            assert set(client.esmtp_features) == {'size', '8bitmime', 'enhancedstatuscodes'}
        checks.append('C01: exact pre/post TLS and AUTH capability sets; unauthenticated submission denied')

        with lab.connect() as client:
            assert client.helo('client.test')[0] == 250
            for option in ['SIZE=1', 'BODY=7BIT', 'BODY=8BITMIME']:
                assert client.docmd('MAIL FROM:<> '+option)[0] == 555
                assert client.docmd('DATA')[0] == 503
            assert client.ehlo()[0] == 250
            for option, code in [('SIZE=000000000000000000000', 501), ('SIZE=18446744073709551616', 552),
                                 ('SIZE=+1', 501), ('SIZE=1 SIZE=1', 501), ('BODY=7BIT BODY=7BIT', 501),
                                 ('BODY=BINARYMIME', 555), ('SMTPUTF8', 555), ('RET=FULL', 555)]:
                assert client.docmd('MAIL FROM:<> '+option)[0] == code, option
            for command in ['MAIL FROM:<"quoted"@example.com>', 'MAIL FROM:<a@[127.0.0.1]>',
                            'MAIL FROM:<@relay.test:a@example.com>', 'RCPT TO:<Postmaster>',
                            'RCPT TO:<>', 'DATA extra', 'VRFY']:
                code, response = client.docmd(command)
                assert code == 501 and response.startswith(b'5.5.2 '), (command, code, response)
            assert client.mail('')[0] == 250
            assert client.rcpt('PoStMaStEr@example.com')[0] == 250
            assert client.mail('', ['SIZE=00000000000000000001', 'body=8bitmime'])[0] == 250
            assert client.rcpt('ALICE@example.com')[0] == 250
            assert client.rcpt('alice@example.com')[0] == 250
            assert client.rcpt('bob@example.com')[0] == 452
            assert client.docmd('HELP', 'MAIL')[0] == 214
            assert client.verify('alice@example.com') == client.verify('missing@example.com')
            assert client.verify('missing@example.com')[0] == 252
            assert client.noop()[0] == 250
            raw = b'Subject: 8bit\r\n\r\n\xff\x80\r\n.dot\r\n'
            code, reply = client.data(raw)
            assert code == 250
            expected[reply.decode().split()[-1]] = (raw, '')
            for reset in ['RSET', 'EHLO reset.test', 'HELO reset.test', 'MAIL FROM:<> SIZE=999999', 'MAIL broken']:
                assert client.ehlo()[0] == 250
                assert client.mail('')[0] == 250
                assert client.rcpt('alice@example.com')[0] == 250
                client.docmd(reset)
                assert client.docmd('DATA')[0] == 503, reset
            assert client.mail('')[0] == 250
            assert client.rcpt('missing@example.com')[0] == 550
            assert client.docmd('DATA')[0] == 503
        checks.append('C02: parameter grammar, HELO negotiation, duplicate limit, HELP/VRFY, failed MAIL and reset isolation')

        # All role/identity/recipient combinations: even authenticated users have
        # no remote relay at M3, and a local-looking sender is not authentication.
        relay_rows = []
        for role, authenticated in [('smtp', False), ('submission', False), ('submission', True), ('submissions', True)]:
            with lab.connect(role, authenticate=authenticated) as client:
                for sender in ['', 'alice@example.com', 'forged@example.com', 'sender@remote.test']:
                    for recipient in ['alice@example.com', 'missing@example.com', 'target@remote.test']:
                        assert client.rset()[0] == 250
                        mail_code = client.mail(sender)[0]
                        expected_mail = 250 if role == 'smtp' or authenticated and sender == 'alice@example.com' else 553 if authenticated else 530
                        assert mail_code == expected_mail, (role, sender, recipient, mail_code)
                        rcpt_code = client.rcpt(recipient)[0]
                        expected_rcpt = (250 if recipient == 'alice@example.com' else 550) if mail_code == 250 else 503 if authenticated else 530
                        assert rcpt_code == expected_rcpt, (role, sender, recipient, rcpt_code)
                        relay_rows.append({'role':role, 'authenticated':authenticated, 'sender':sender,
                                           'recipient':recipient, 'mail':mail_code, 'rcpt':rcpt_code})
        checks.append('C03: 48 role/sender/recipient combinations deny open relay and unauthorized send-as')

        for verb, limit in [(b'NOOP ', 512), (b'MAIL FROM:<> ', 538)]:
            for length in [limit, limit+1]:
                client = lab.connect()
                try:
                    frame = verb+b' '*(length-len(verb)-2)+b'\r\n'
                    assert len(frame) == length
                    client.sock.sendall(frame)
                    assert client.getreply()[0] == (250 if length == limit else 500)
                    if length > limit:
                        closed(client)
                finally:
                    client.close()
        for frame in [b'NOOP\n', b'NOOP\rX\r\n', b'NO\0OP\r\n']:
            client = lab.connect()
            try:
                client.sock.sendall(frame)
                assert client.getreply()[0] == 500
                closed(client)
            finally:
                client.close()
        checks.append('C04: 512/538 command limits include CRLF; NUL and bare CR/LF close the connection')

        bad_data = [b'\r\n8bit \xff\r\n', b'\r\nNUL\0\r\n', b'\r\nbare\n.\nNOOP\r\n',
                    b'\r\nbare\r.\rNOOP\r\n', b'\r\n'+b'x'*999+b'\r\n', b'\r\n.'+b'x'*999+b'\r\n']
        for wire in bad_data:
            client = lab.connect()
            try:
                envelope(client)
                client.sock.sendall(wire+b'.\r\nNOOP\r\n')
                assert client.getreply()[0] == 554, wire[:30]
                closed(client)
            finally:
                client.close()
        client = lab.connect()
        try:
            envelope(client, ['BODY=8BITMIME'])
            client.sock.sendall(b'Subject: \xff\r\n\r\n.\r\n')
            assert client.getreply()[0] == 550
            closed(client)
        finally:
            client.close()
        checks.append('C05: undeclared high-bit data, binary NUL, smuggling delimiters, overlong lines and SMTPUTF8 headers rejected without suffix execution')

        with lab.connect() as client:
            envelope(client)
            raw = b'Subject: split\r\n\r\n'+b'.'+b'x'*997+b'\r\nDATA\r\n'
            wire = raw.replace(b'\r\n.', b'\r\n..')+b'.\r\n'
            # TCP may coalesce writes; the pure decoder tests guarantee every
            # split. This independently exercises real transport ordering.
            for byte in wire:
                client.sock.sendall(bytes([byte]))
            code, reply = client.getreply()
            assert code == 250
            expected[reply.decode().split()[-1]] = (raw, 'sender@remote.test')
            client.sock.sendall(b'NOOP\r\nRSET\r\nHELP\r\n')
            assert [client.getreply()[0] for _ in range(3)] == [250, 250, 214]
        checks.append('C06: bytewise DATA writes, 1001-byte stuffed wire line and coalesced commands preserve content/order')

        client = lab.connect()
        try:
            envelope(client)
            started = time.monotonic()
            # Bytes without CRLF do not reset the line deadline.
            for _ in range(4):
                client.sock.sendall(b'x')
                time.sleep(.55)
            assert client.getreply()[0] == 421
            assert time.monotonic()-started < 6
            closed(client)
        finally:
            client.close()
        checks.append('C07: slow DATA line deadline is absolute and releases the connection')

        integrity = lab.check_store(len(expected))
        for message_id, (raw, sender) in expected.items():
            destination = lab.base/(message_id+'.eml')
            lab.ctl('mail', 'export', 'alice@example.com', message_id, '--output', str(destination))
            retained, _ = delivery_content(destination.read_bytes(), sender, 'ESMTP')
            assert retained == raw
        assert '8bit' not in lab.log_path.read_text()
        checks.append('C08: reopened store has only acknowledged messages; retained bytes exact; staging empty; secrets absent from logs')
        report(args.output, checks=checks, relay_matrix=relay_rows, accepted_messages=len(expected), integrity=integrity)


if __name__ == '__main__':
    main()
