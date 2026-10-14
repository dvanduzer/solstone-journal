#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
import json
from pathlib import Path
import queue
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from qualify_serving import Phone


class ReceiptTests(unittest.TestCase):
    def launch(self, stored):
        start = {'event': 'host.start', 'host_pid': 99, 'time': 2000, 'uptime': 10,
                 'value': {'journal': 'receipt-test'}}
        served = {'event': 'serve.start', 'host_pid': 99, 'time': 2001, 'uptime': 11,
                  'value': {'reply': {'journal': {'pid': 99, 'result': {'ok': True, 'value': {}}}}}}
        with tempfile.TemporaryDirectory() as temporary:
            phone = Phone(SimpleNamespace(output=Path(temporary), device='fixture',
                                          suite='receipt-test', port=37657))

            def lost_console(self, predicate, seconds=160):
                while self.events.get(timeout=2) is not None:
                    pass
                raise queue.Empty()

            def copy(command, **kwargs):
                self.assertEqual(command[4], 'from')
                target = Path(command[command.index('--destination') + 1])
                target.write_text(''.join(json.dumps(r) + '\n' for r in stored(start, served)))

            process = SimpleNamespace(stdout=[json.dumps(start) + '\n'])
            try:
                with patch('qualify_serving.subprocess.Popen', return_value=process), \
                        patch('qualify_serving.subprocess.run', side_effect=copy), \
                        patch.object(Phone, 'wait', lost_console):
                    return phone.launch()
            finally:
                phone.log.close()

    def test_fresh_durable_launch_receipt_is_accepted(self):
        actual = self.launch(lambda start, served: [start, served])
        self.assertEqual(actual['host_pid'], 99)

    def test_pre_reboot_same_pid_and_higher_uptime_cannot_qualify(self):
        def rows(start, served):
            old = {**served, 'time': 1000, 'uptime': 900}
            return [old, start]
        with self.assertRaisesRegex(AssertionError, 'startup receipt'):
            self.launch(rows)

    def test_duplicate_launch_identity_refuses(self):
        with self.assertRaisesRegex(AssertionError, 'durable launch'):
            self.launch(lambda start, served: [start, served, start])

    def test_another_process_cannot_supply_startup(self):
        with self.assertRaisesRegex(AssertionError, 'startup receipt'):
            self.launch(lambda start, served: [start, {**served, 'host_pid': 100}])


if __name__ == '__main__':
    unittest.main()
