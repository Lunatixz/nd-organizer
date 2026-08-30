#!/usr/bin/env python3
with open(r'D:\GitHub\nd-organizer\webhook\server.py') as f:
    content = f.read()
import re
btn = content.find('forceRescan()')
func = content.find('function forceRescan')
print(f'Button at char {btn}, function at char {func}')
if btn < func:
    print('Button BEFORE function - onclick fires before JS defined')
else:
    print('Button AFTER function - OK')
