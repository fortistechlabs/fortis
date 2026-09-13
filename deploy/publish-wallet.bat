@echo off
REM Publish web\ to Cloudflare Pages. Double-click, or run from any shell.
REM All the logic lives in publish-wallet.ps1; args pass straight through
REM   (e.g.  publish-wallet.bat -Preview  /  publish-wallet.bat -Project fortis).
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0publish-wallet.ps1" %*
