#!/usr/bin/env bash
# Make bitcoind RPC credentials for fortis-lsp (user "fortis-lsp").
#   deploy/linux-home/gen-lsp-rpcauth.sh OUTDIR
# Writes OUTDIR/lsp-rpc-password (plaintext, 0600) and OUTDIR/rpcauth.line (the
# line for bitcoin.conf, which holds only a salted HMAC, not the password).
# Nothing is installed or changed; see deploy/lightning.md §3.
set -euo pipefail
out=${1:?usage: gen-lsp-rpcauth.sh OUTDIR}
umask 077
mkdir -p "$out"
python3 -I - "$out" <<'PY'
import hmac, hashlib, secrets, sys
out = sys.argv[1]
user = "fortis-lsp"
pw = secrets.token_urlsafe(32)
salt = secrets.token_hex(16)
h = hmac.new(salt.encode(), pw.encode(), hashlib.sha256).hexdigest()
open(f"{out}/lsp-rpc-password", "w").write(pw + "\n")
open(f"{out}/rpcauth.line", "w").write(f"rpcauth={user}:{salt}${h}\n")
PY
chmod 0600 "$out/lsp-rpc-password" "$out/rpcauth.line"
echo "wrote $out/lsp-rpc-password and $out/rpcauth.line"
