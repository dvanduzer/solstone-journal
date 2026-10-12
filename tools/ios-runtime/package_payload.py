#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Stage the maintained router payload as a signed helper bundle resource."""
from pathlib import Path
import shutil

adapter = Path(__file__).resolve().parent
source = adapter.parents[1] / 'core/payload/solstone'
destination = adapter / 'Resources/JournalPayload'
for marker in ('talent/journal/contract/bundle.json', 'think/contract/layout.json',
               'think/templates/segment_preamble.md'):
    assert (source / marker).is_file(), f'missing payload marker: {marker}'
if destination.exists():
    shutil.rmtree(destination)
shutil.copytree(source, destination / 'solstone')
