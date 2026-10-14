#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Host and device qualification observer for iOS GPU appraisal.

Opt-in qualification harness with pure verdict evaluation.
No device required for unit tests. No sleep in the verdict function.
"""

from typing import Any, Dict, List, Optional
import unittest


def verdict(observation: Dict[str, Any]) -> bool:
    """Evaluates qualification observations for an appraisal attempt.

    Succeeds only when observations include the helper pid, a reply, and
    the later death of that same pid, for both an accepted and a refused result.
    Missing any of those refuses.
    """
    pid = observation.get("pid")
    if not isinstance(pid, int) or pid <= 0:
        return False

    reply = observation.get("reply")
    if not isinstance(reply, dict):
        return False

    # Later death of that same pid is strictly required
    exited_pid = observation.get("exited_pid")
    if exited_pid != pid:
        return False

    if not observation.get("exited_after_reply", False):
        return False

    if observation.get("main_thread_init") is not True:
        return False

    return True


def evaluate_session(observations: List[Dict[str, Any]]) -> bool:
    """Evaluates a full qualification run covering acceptance criteria."""
    if not observations:
        return False

    seen_pids = set()
    for obs in observations:
        if not verdict(obs):
            return False
        pid = obs["pid"]
        if pid in seen_pids:
            # Must use a fresh pid per appraisal
            return False
        seen_pids.add(pid)

    return True


class TestQualifyGpuAppraisalVerdict(unittest.TestCase):
    def test_accepted_with_pid_reply_and_exit_succeeds(self):
        obs = {
            "pid": 1234,
            "main_thread_init": True,
            "reply": {
                "returncode": 0,
                "result_code": 0,
                "result_message": "Ok",
            },
            "exited_pid": 1234,
            "exited_after_reply": True,
        }
        self.assertTrue(verdict(obs))

    def test_refused_with_pid_reply_and_exit_succeeds(self):
        obs = {
            "pid": 5678,
            "main_thread_init": True,
            "reply": {
                "returncode": 0,
                "result_code": 504,
                "result_message": "Nonce mismatch",
            },
            "exited_pid": 5678,
            "exited_after_reply": True,
        }
        self.assertTrue(verdict(obs))

    def test_missing_pid_refuses(self):
        obs = {
            "reply": {"returncode": 0, "result_code": 0},
            "exited_pid": 1234,
            "exited_after_reply": True,
        }
        self.assertFalse(verdict(obs))

    def test_missing_reply_refuses(self):
        obs = {
            "pid": 1234,
            "exited_pid": 1234,
            "exited_after_reply": True,
        }
        self.assertFalse(verdict(obs))

    def test_pid_mismatch_on_exit_refuses(self):
        obs = {
            "pid": 1234,
            "reply": {"returncode": 0, "result_code": 0},
            "exited_pid": 9999,
            "exited_after_reply": True,
        }
        self.assertFalse(verdict(obs))

    def test_process_did_not_exit_after_reply_refuses(self):
        obs = {
            "pid": 1234,
            "reply": {"returncode": 0, "result_code": 0},
            "exited_pid": 1234,
            "exited_after_reply": False,
        }
        self.assertFalse(verdict(obs))

    def test_main_thread_init_false_refuses(self):
        obs = {
            "pid": 1234,
            "main_thread_init": False,
            "reply": {"returncode": 0, "result_code": 0},
            "exited_pid": 1234,
            "exited_after_reply": True,
        }
        self.assertFalse(verdict(obs))

    def test_missing_main_thread_init_refuses(self):
        obs = {
            "pid": 1234,
            "reply": {"returncode": 0, "result_code": 0},
            "exited_pid": 1234,
            "exited_after_reply": True,
        }
        self.assertFalse(verdict(obs))

    def test_cancelled_without_death_after_reply_refuses(self):
        obs = {
            "pid": 2222,
            "cancelled": True,
            "reply": {"returncode": 1, "result_code": 1, "result_message": "cancelled"},
            "exited_pid": 2222,
            "exited_after_reply": False,
        }
        self.assertFalse(verdict(obs))

    def test_cancelled_with_death_after_reply_succeeds(self):
        obs = {
            "pid": 2222,
            "main_thread_init": True,
            "cancelled": True,
            "reply": {"returncode": 1, "result_code": 1, "result_message": "cancelled"},
            "exited_pid": 2222,
            "exited_after_reply": True,
        }
        self.assertTrue(verdict(obs))

    def test_watchdog_60s_kill_requires_death_after_reply(self):
        obs = {
            "pid": 3333,
            "cancelled": True,
            "reply": {"returncode": 124, "result_code": 124, "result_message": "deadline"},
            "exited_pid": 3333,
            "exited_after_reply": False,
        }
        self.assertFalse(verdict(obs))

        obs_with_exit = {
            "pid": 3333,
            "main_thread_init": True,
            "cancelled": True,
            "reply": {"returncode": 124, "result_code": 124, "result_message": "deadline"},
            "exited_pid": 3333,
            "exited_after_reply": True,
        }
        self.assertTrue(verdict(obs_with_exit))

    def test_empty_session_is_false(self):
        self.assertFalse(evaluate_session([]))

    def test_multi_attempt_requires_fresh_pid(self):
        obs1 = {
            "pid": 1001,
            "main_thread_init": True,
            "reply": {"returncode": 0, "result_code": 0},
            "exited_pid": 1001,
            "exited_after_reply": True,
        }
        obs2 = {
            "pid": 1002,
            "main_thread_init": True,
            "reply": {"returncode": 0, "result_code": 0},
            "exited_pid": 1002,
            "exited_after_reply": True,
        }
        self.assertTrue(evaluate_session([obs1, obs2]))

        # Reusing the same pid fails
        obs2_reuse = {
            "pid": 1001,
            "main_thread_init": True,
            "reply": {"returncode": 0, "result_code": 0},
            "exited_pid": 1001,
            "exited_after_reply": True,
        }
        self.assertFalse(evaluate_session([obs1, obs2_reuse]))


if __name__ == "__main__":
    unittest.main()
