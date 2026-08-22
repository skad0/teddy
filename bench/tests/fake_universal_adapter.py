#!/usr/bin/env python3
"""Deterministic PTY participant for universal-contract tests."""
import os, sys, time

child=os.fork()
if child==0:
    time.sleep(2)
    os._exit(0)
os.write(1,b"\033[6n\033[c")
os.write(1,b"UBENCH_HEAD_RECORD\n")
line=sys.stdin.readline()
if line:
    os.write(1,b"/UBENCH_NEEDLE\nUBENCH_NEEDLE\n")
    time.sleep(.02)
    os.write(1,b"UBENCH_TARGET_RECORD\n")
try: os.waitpid(child,0)
except ChildProcessError: pass
