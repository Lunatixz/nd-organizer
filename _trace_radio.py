#!/usr/bin/env python3
"""Trace the radio Add button flow through the code."""
import re

with open(r'D:\GitHub\nd-organizer\webhook\server.py') as f:
    code = f.read()
    lines = code.split('\n')

print("=== 1. JavaScript onclick handler ===")
for i, line in enumerate(lines):
    if 'radioAdd(' in line and 'onclick' in line:
        print(f"  Line {i+1}: {line.strip()[:120]}")

print("\n=== 2. JavaScript radioAdd function ===")
for i, line in enumerate(lines):
    if 'function radioAdd' in line:
        for j in range(i, min(i+8, len(lines))):
            print(f"  Line {j+1}: {lines[j].rstrip()[:120]}")
        break

print("\n=== 3. Server-side /radio-add handler ===")
for i, line in enumerate(lines):
    if 'radio-add' in line and 'endswith' in line:
        for j in range(i, min(i+25, len(lines))):
            print(f"  Line {j+1}: {lines[j].rstrip()[:120]}")
        break

print("\n=== 4. Radio sidecar /add endpoint ===")
with open(r'D:\GitHub\nd-organizer\radio\server.py') as f:
    rcode = f.read()
    rlines = rcode.split('\n')
    for i, line in enumerate(rlines):
        if '/add' in line and 'endpoints' not in line.lower():
            for j in range(max(0,i-2), min(i+15, len(rlines))):
                print(f"  radio Line {j+1}: {rlines[j].rstrip()[:120]}")
            break
