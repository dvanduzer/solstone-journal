// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
package app.solstone.development.journal;

public final class ServingConnection {
    public enum Action {
        DROP,
        RETRY,
        REFUSE,
        ENABLE_AUDIO,
        READY
    }

    public enum Retirement {
        PENDING,
        STOPPED,
        FAILED
    }

    public interface Environment {
        long currentGeneration();
        Retirement retirement() throws Exception;
        boolean start() throws Exception;
        boolean enableAudio() throws Exception;
        void onReady(long generation);
    }

    public static Action decide(
        long commandGeneration,
        long currentGeneration,
        boolean startAlreadySucceeded,
        Boolean startOk,
        Boolean audioOk,
        Retirement retirement
    ) {
        if (commandGeneration != currentGeneration) {
            return Action.DROP;
        }
        if (retirement == Retirement.FAILED || Boolean.FALSE.equals(startOk)) {
            return Action.REFUSE;
        }
        if (Boolean.FALSE.equals(audioOk)) {
            return Action.REFUSE;
        }
        if (retirement == Retirement.PENDING && !startAlreadySucceeded) {
            return Action.RETRY;
        }
        if (retirement == Retirement.PENDING && startAlreadySucceeded) {
            return Action.RETRY;
        }
        if (startAlreadySucceeded || Boolean.TRUE.equals(startOk)) {
            if (audioOk == null) {
                return Action.ENABLE_AUDIO;
            }
            if (Boolean.TRUE.equals(audioOk)) {
                return Action.READY;
            }
        }
        if (retirement == Retirement.STOPPED && !startAlreadySucceeded) {
            return Action.RETRY;
        }
        return Action.REFUSE;
    }

    public static Action drive(long generation, boolean startAlreadySucceeded, Environment env) throws Exception {
        if (generation != env.currentGeneration()) {
            return Action.DROP;
        }
        Retirement ret = env.retirement();
        if (ret == Retirement.FAILED) {
            return Action.REFUSE;
        }
        if (ret == Retirement.PENDING) {
            return Action.RETRY;
        }
        if (generation != env.currentGeneration()) {
            return Action.DROP;
        }
        if (!startAlreadySucceeded) {
            boolean startOk = env.start();
            if (!startOk) {
                return Action.REFUSE;
            }
            startAlreadySucceeded = true;
        }
        if (generation != env.currentGeneration()) {
            return Action.DROP;
        }
        boolean audioOk = env.enableAudio();
        if (!audioOk) {
            return Action.REFUSE;
        }
        if (generation != env.currentGeneration()) {
            return Action.DROP;
        }
        env.onReady(generation);
        return Action.READY;
    }

    public static Action drive(long generation, Environment env) throws Exception {
        return drive(generation, false, env);
    }
}
