@echo off
REM Publish site\ to Cloudflare Pages. Double-click, or run from any shell.
REM All the logic lives in publish-site.ps1; args pass straight through
REM   (e.g.  publish-site.bat -Preview  /  publish-site.bat -Project fortis).
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0publish-site.ps1" %*
