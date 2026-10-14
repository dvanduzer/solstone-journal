// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
package app.solstone.development.journal;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.ArrayList;
import java.util.Arrays;

// Standalone host lifecycle check; no Android or provider stand-in is involved.
public final class ThinkingAdmissionTest {
    static void require(boolean value) { if (!value) throw new AssertionError(); }
    public static void main(String[] args) throws Exception {
        ThinkingAdmission gate = new ThinkingAdmission();
        require(gate.admit() == null);
        long first = gate.generation(); gate.opened(first);
        ThinkingAdmission.Ticket active = gate.admit();
        require(active != null && active.current());
        require(gate.admit() == null); // No second request can enter an executor queue.
        gate.stop(); require(!active.current()); require(gate.admit() == null);
        gate.opened(first); require(gate.admit() == null); // A stale start cannot reopen.
        CountDownLatch waiting = new CountDownLatch(1);
        FutureTask<Void> stop = new FutureTask<>(()->{ waiting.countDown(); gate.awaitIdle(); return null; });
        Thread thread = new Thread(stop); thread.start(); waiting.await();
        try { stop.get(100, TimeUnit.MILLISECONDS); throw new AssertionError("retired active request"); }
        catch (TimeoutException expected) { /* The accepted owner is still held. */ }
        active.close(); active.close(); // Repeated release must not mint another permit.
        stop.get(5, TimeUnit.SECONDS); thread.join();
        gate.opened(gate.generation());
        ThinkingAdmission.Ticket fresh = gate.admit();
        require(fresh != null && fresh.current() && !active.current());
        require(gate.admit() == null);
        fresh.close(); gate.stop(); gate.awaitIdle();

        // Two Java service lifetimes share the actual process execution owner.
        // Hold its queue to reproduce an old explicit stop and destruction
        // arriving before a replacement service submits startup.
        JournalExecution execution = new JournalExecution();
        CountDownLatch queueHeld = new CountDownLatch(1), releaseQueue = new CountDownLatch(1);
        AtomicInteger running = new AtomicInteger(1);
        ArrayList<String> order = new ArrayList<>();
        execution.work.execute(()->{
            queueHeld.countDown();
            try { releaseQueue.await(); }
            catch (InterruptedException e) { throw new AssertionError(e); }
        });
        queueHeld.await();
        execution.admission.opened(execution.admission.generation());
        ThinkingAdmission.Ticket old = execution.admission.admit();
        execution.admission.stop();
        execution.work.execute(()->{
            execution.admission.awaitIdle(); running.set(0); order.add("explicit stop");
        });
        execution.retire(()->{ running.set(0); order.add("old destruction"); });
        long replacementGeneration = execution.admission.generation();
        Future<?> replacement = execution.work.submit(()->{
            running.set(2); order.add("replacement start");
            execution.admission.opened(replacementGeneration);
        });
        releaseQueue.countDown();
        try { replacement.get(100, TimeUnit.MILLISECONDS); throw new AssertionError("replaced active request"); }
        catch (TimeoutException expected) { /* The old owner still holds its request. */ }
        old.close(); replacement.get(5, TimeUnit.SECONDS);
        execution.work.submit(()->{}).get(5, TimeUnit.SECONDS);
        require(running.get() == 2);
        require(order.equals(Arrays.asList("explicit stop", "old destruction", "replacement start")));
        ThinkingAdmission.Ticket replacementTicket = execution.admission.admit();
        require(replacementTicket != null && replacementTicket.current());
        replacementTicket.close();
        execution.work.shutdown(); execution.thinking.shutdown();
        System.out.println("thinking admission lifecycle passed");
    }
}
