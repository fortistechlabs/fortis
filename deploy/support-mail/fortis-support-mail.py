#!/usr/bin/env python3
"""Emails each new Fortis support ticket (fortis-edge `POST /support`) to the operator.

Run by fortis-support-mail.service, started by the .path unit when a ticket file appears
(and by the .timer every 10 minutes, to retry any that could not be sent). Sends through
msmtp (config: /etc/fortis/msmtprc, account "fortis"). A ticket is marked sent only once
msmtp accepts it, so none are lost when the network is down.

Usage: fortis-support-mail.py [--test]
"""
import json
import os
import subprocess
import sys
import time
from email.message import EmailMessage
from pathlib import Path

TICKETS = Path("/var/lib/fortis/edge/support")
STATE = Path("/var/lib/fortis-support-mail/sent")
TO = "donaldevine@gmail.com"
MSMTP = ["msmtp", "--file=/etc/fortis/msmtprc", "--account=fortis", TO]


def send(msg: EmailMessage) -> None:
    msg["To"] = TO
    msg["From"] = TO
    subprocess.run(MSMTP, input=msg.as_bytes(), check=True, timeout=60)


def summary(doc: dict) -> str:
    if not isinstance(doc, dict):
        doc = {}
    t = doc.get("ticket")
    if not isinstance(t, dict):
        t = {}
    d = t.get("diagnostics")
    if not isinstance(d, dict):
        d = {}
    received_at = doc.get("received_at", 0)
    if not isinstance(received_at, (int, float)):
        received_at = 0
    wallets = d.get("wallets")
    if not isinstance(wallets, list):
        wallets = []
    lines = [
        f"Ticket:   {doc.get('id')}",
        f"Received: {time.strftime('%Y-%m-%d %H:%M:%S %Z', time.localtime(received_at))}",
        f"Install:  {doc.get('install')}",
        f"Reply to: {t.get('email') or '(none given)'}",
        f"App:      {t.get('app') or ''}",
    ]
    if d:
        lines += [
            f"Phone:    {d.get('device') or ''} · Android {d.get('android') or ''} · {d.get('locale') or ''} · {d.get('time_zone') or ''}",
            f"Wallets:  {', '.join(str(w) for w in wallets)}",
            f"Lightning: {d.get('lightning_state') or ''}",
            f"Account:  {d.get('account') or ''}",
        ]
    lines += ["", "Message:", str(t.get("message") or ""), ""]
    errors = d.get("recent_errors")
    if isinstance(errors, list) and errors:
        lines += ["Recent errors (newest last):"] + [f"  {e}" for e in errors[-15:]] + [""]
    if not d:
        lines += ["(The user did not include diagnostic logs.)"]
    else:
        lines += ["The full ticket, with the Lightning node log, is attached."]
    return "\n".join(lines)


def read_ticket_bytes(path: Path) -> bytes:
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(fd, "rb") as fh:
        return fh.read()


def main() -> int:
    if "--test" in sys.argv:
        m = EmailMessage()
        m["Subject"] = "Fortis support mail: test"
        m.set_content("If you can read this, support tickets will be emailed to you.")
        send(m)
        print("test email sent")
        return 0
    STATE.parent.mkdir(parents=True, exist_ok=True)
    sent = set(STATE.read_text().split()) if STATE.exists() else set()
    if not TICKETS.is_dir():
        return 0
    failed = 0
    for f in sorted(TICKETS.glob("FT-*.json"), key=lambda p: p.lstat().st_mtime):
        if f.stem in sent:
            continue
        try:
            raw = read_ticket_bytes(f)
            doc = json.loads(raw.decode("utf-8"))
        except (OSError, ValueError):
            continue  # still being written or invalid symlink: the next run picks it up
        try:
            if not isinstance(doc, dict):
                raise ValueError("ticket root must be a JSON object")
            ticket = doc.get("ticket") if isinstance(doc.get("ticket"), dict) else {}
            m = EmailMessage()
            msg = str(ticket.get("message") or "").strip().splitlines()
            first_line = (msg[0] if msg else "").replace("\r", " ").replace("\n", " ")[:70]
            ticket_id = str(doc.get("id") or f.stem).replace("\r", "").replace("\n", "")
            m["Subject"] = f"Fortis support {ticket_id}: {first_line}"
            m.set_content(summary(doc))
            m.add_attachment(raw, maintype="application", subtype="json", filename=f.name)
            send(m)
        except Exception as e:
            print(f"{f.stem}: not sent ({e}); will retry", file=sys.stderr)
            failed += 1
            continue
        with STATE.open("a") as s:
            s.write(f.stem + "\n")
        print(f"{f.stem}: emailed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
