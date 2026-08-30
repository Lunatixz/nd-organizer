#!/usr/bin/env python3
"""Trace the scan pipeline flow."""
import re

with open(r'D:\GitHub\nd-organizer\src\scan.rs') as f:
    code = f.read()

# Find index_file and check mtime handling
for i, line in enumerate(code.split('\n')):
    if 'mtime' in line and ('==' in line or 'get' in line):
        print(f"Line {i+1}: {line.strip()[:120]}")

print("\n=== scan_step outcome handling ===")
for i, line in enumerate(code.split('\n')):
    if 'ScanOutcome' in line or 'hit_limit' in line or 'capped' in line:
        print(f"Line {i+1}: {line.strip()[:120]}")

print("\n=== enqueue_scan_task / enqueue_group_task ===")
import re
with open(r'D:\GitHub\nd-organizer\src\lib.rs') as f:
    libcode = f.read()
for i, line in enumerate(libcode.split('\n')):
    if 'enqueue_scan' in line or 'enqueue_group' in line or 'scan_done' in line:
        print(f"lib Line {i+1}: {line.strip()[:120]}")
