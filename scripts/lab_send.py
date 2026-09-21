#!/usr/bin/env python3
"""Send a synthetic message to a rustymail lab receiver on loopback only."""
import argparse
from email.message import EmailMessage
from email.policy import SMTP
import smtplib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=2525)
    parser.add_argument("--to", default="alice@example.com")
    args = parser.parse_args()
    message = EmailMessage(policy=SMTP)
    message["From"] = "sender@remote.test"
    message["To"] = args.to
    message["Subject"] = "rustymail: first local message"
    message.set_content("Hello, rustymail!\n这是本地合成测试邮件。\n.leading dot\n")
    with smtplib.SMTP("127.0.0.1", args.port, timeout=30) as client:
        refused = client.send_message(message, mail_options=['BODY=8BITMIME'])
        if refused:
            raise RuntimeError(f"Recipients refused: {refused}")
    print("SMTP final 250 received; stop the lab server before listing/exporting mail.")


if __name__ == "__main__":
    main()
