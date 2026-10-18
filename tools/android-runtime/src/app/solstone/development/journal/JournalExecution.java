// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
package app.solstone.development.journal;

import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

// Native serving state belongs to the process. Service recreation uses this
// same owner, so old retirement and replacement startup share one FIFO.
final class JournalExecution {
    static final JournalExecution PROCESS = new JournalExecution();
    final ExecutorService work = Executors.newSingleThreadExecutor();
    final ExecutorService thinking = Executors.newSingleThreadExecutor();
    final ThinkingAdmission admission = new ThinkingAdmission();

    void beginStop(Runnable cancel) {
        admission.stop();
        cancel.run();
    }

    void retire(Runnable cancel, Runnable stop) {
        beginStop(cancel);
        work.execute(() -> {
            admission.awaitIdle();
            stop.run();
        });
    }
}
