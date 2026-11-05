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
    t = doc.get("ticket", {})
    d = t.get("diagnostics") or {}
    lines = [
        f"Ticket:   {doc.get('id')}",
        f"Received: {time.strftime('%Y-%m-%d %H:%M:%S %Z', time.localtime(doc.get('received_at', 0)))}",
        f"Install:  {doc.get('install')}",
        f"Reply to: {t.get('email') or '(none given)'}",
        f"App:      {t.get('app', '')}",
    ]
    if d:
        lines += [
            f"Phone:    {d.get('device', '')} · Android {d.get('android', '')} · {d.get('locale', '')} · {d.get('time_zone', '')}",
            f"Wallets:  {', '.join(d.get('wallets', []))}",
            f"Lightning: {d.get('lightning_state', '')}",
            f"Account:  {d.get('account', '')}",
        ]
    lines += ["", "Message:", t.get("message", ""), ""]
    errors = d.get("recent_errors") or []
    if errors:
        lines += ["Recent errors (newest last):"] + [f"  {e}" for e in errors[-15:]] + [""]
    if not d:
        lines += ["(The user did not include diagnostic logs.)"]
    else:
        lines += ["The full ticket, with the Lightning node log, is attached."]
    return "\n".join(lines)


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
    for f in sorted(TICKETS.glob("FT-*.json"), key=lambda p: p.stat().st_mtime):
        if f.stem in sent:
            continue
        try:
            doc = json.loads(f.read_text())
        except (OSError, ValueError):
            continue  # still being written: the next run picks it up
        m = EmailMessage()
        msg = (doc.get("ticket", {}).get("message") or "").strip().splitlines()
        m["Subject"] = f"Fortis support {doc.get('id')}: {(msg[0] if msg else '')[:70]}"
        m.set_content(summary(doc))
        m.add_attachment(f.read_bytes(), maintype="application", subtype="json", filename=f.name)
        try:
            send(m)
        except (subprocess.SubprocessError, OSError) as e:
            print(f"{f.stem}: not sent ({e}); will retry", file=sys.stderr)
            failed += 1
            continue
        with STATE.open("a") as s:
            s.write(f.stem + "\n")
        print(f"{f.stem}: emailed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
