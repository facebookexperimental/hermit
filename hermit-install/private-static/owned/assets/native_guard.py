"""Existing bounded metadata helpers; no standalone historical CLI."""
import argparse


import hashlib


import importlib.util


import json


import os


from pathlib import Path


import re


import stat


import shutil


import subprocess


import sys


import time


def need(ok, message):
    if not ok:
        raise RuntimeError(message)


def digest(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def private_guard(recipe):
    prefix = Path(recipe['toolchain']['prefix'])
    receipt_path = Path(recipe['toolchain']['input_inventory'])
    inventory = json.loads(receipt_path.read_text())
    need(inventory['private_prefix'] == str(prefix), 'private prefix authority mismatch')
    expected = dict(inventory['copied_host_inputs'])
    need(not set(expected) & set(inventory['added_target_inputs']), 'inventory collision')
    expected.update(inventory['added_target_inputs'])
    observed = set()
    for root, dirs, files in os.walk(prefix, followlinks=False):
        need(not any((Path(root) / n).is_symlink() for n in dirs), 'private directory symlink')
        for name in files:
            p = Path(root) / name
            rel = p.relative_to(prefix).as_posix()
            need(p.is_file() and not p.is_symlink(), 'private input not regular: ' + rel)
            need(rel in expected, 'unbound private input: ' + rel)
            row = expected[rel]
            need(p.stat().st_size == row['bytes'] and digest(p) == row['sha256'], 'private input hash changed: ' + rel)
            if 'mode' in row:
                need(stat.S_IMODE(p.stat().st_mode) == row['mode'], 'private input mode changed: ' + rel)
            observed.add(rel)
    need(observed == set(expected), 'private inventory file set changed')
    return {'inventory_sha256': digest(receipt_path), 'files_fully_hashed': len(observed),
            'bytes_fully_hashed': sum(row['bytes'] for row in expected.values())}
