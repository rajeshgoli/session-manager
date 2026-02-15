#!/usr/bin/env python3
"""Test script to reproduce issue #78: sm clear fails on completed sessions."""

import subprocess
import time
import sys

def run_cmd(cmd):
    """Run a command and return stdout, stderr, returncode."""
    result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    return result.stdout, result.stderr, result.returncode

# Test Case 1: Create a child session that completes immediately
print("Test Case 1: Create a child session that will complete")
print("=" * 60)

# Spawn a child with a simple task that completes
stdout, stderr, rc = run_cmd('sm spawn "just say hello" --name test-clear-child')
print(f"Spawn command output:\n{stdout}")
if stderr:
    print(f"Stderr: {stderr}")

# Extract session ID from output
if "(" in stdout and ")" in stdout:
    # Parse session ID from "Spawned test-clear-child (session-id) in tmux session ..."
    session_id = stdout.split("(")[1].split(")")[0]
    print(f"\nChild session ID: {session_id}")

    # Wait for it to complete
    print("\nWaiting for child to complete (5 seconds)...")
    time.sleep(5)

    # Check status
    stdout, stderr, rc = run_cmd(f'sm children')
    print(f"\nChildren status:\n{stdout}")

    # Try to clear it (THIS SHOULD FAIL with "not in a mode")
    print(f"\nAttempting to clear session {session_id}...")
    stdout, stderr, rc = run_cmd(f'sm clear {session_id}')
    print(f"Exit code: {rc}")
    print(f"Stdout: {stdout}")
    print(f"Stderr: {stderr}")

    if "not in a mode" in stderr:
        print("\n✓ REPRODUCED: 'not in a mode' error confirmed")
    else:
        print("\n✗ Could not reproduce the error")

else:
    print("ERROR: Could not extract session ID from spawn output")
    sys.exit(1)
