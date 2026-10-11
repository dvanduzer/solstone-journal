// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
package app.solstone.experiment.runtime;
public final class Native {
    static { System.loadLibrary("journalbridge"); }
    public static native String run(String root, String op, int count);
}
