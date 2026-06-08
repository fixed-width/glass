#!/usr/bin/env bash
# set_autologin.sh — enable macOS auto-login from a pure SSH session (no GUI/VNC).
#
# Workaround for `sysadminctl -autologin set` failing on Apple Silicon
# (SACSetAutoLoginPassword error:22): it sets `autoLoginUser` but never writes
# /etc/kcpassword, so the box stops at the loginwindow. This writes kcpassword
# directly (the obfuscated-password file the loginwindow reads) using the well-known
# XOR scheme, plus sets autoLoginUser. Requires FileVault OFF.
#
#   ./set_autologin.sh [username]      # default: current user
#
# Your password is read with `read -s` (never echoed, never in argv/process list).
# kcpassword bytes are computed locally, then written to /etc/kcpassword via `sudo tee`.
set -euo pipefail

USER_NAME="${1:-$(id -un)}"

read -rs -p "Login password for $USER_NAME: " GLASS_PW; echo
export GLASS_PW

# Encode: XOR password (padded to a multiple of 12 with NULs) against the fixed cipher.
python3 -c '
import os, sys
pw  = os.environ["GLASS_PW"].encode("utf-8")
key = bytes([0x7D,0x89,0x52,0x23,0xD2,0xBC,0xDD,0xEA,0xA3,0xB9,0x1F])
pad = 12 - (len(pw) % 12)            # always 1..12 → always at least one NUL terminator
pw += b"\x00" * pad
sys.stdout.buffer.write(bytes(pw[i] ^ key[i % 11] for i in range(len(pw))))
' | sudo tee /etc/kcpassword > /dev/null

sudo chmod 600 /etc/kcpassword
sudo chown root:wheel /etc/kcpassword
sudo defaults write /Library/Preferences/com.apple.loginwindow autoLoginUser "$USER_NAME"

unset GLASS_PW
echo "auto-login set for $USER_NAME (kcpassword written, autoLoginUser set)."
echo "Reboot and the Aqua session should come up unattended (FileVault must be off)."
