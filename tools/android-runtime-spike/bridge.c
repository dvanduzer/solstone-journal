// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#include <jni.h>
#include <stddef.h>
extern char *spike_run(const char *, const char *, size_t);
extern void spike_free(char *);
JNIEXPORT jstring JNICALL Java_app_solstone_experiment_runtime_Native_run(JNIEnv *e, jclass c, jstring r, jstring o, jint n) {
    (void)c;
    const char *root=(*e)->GetStringUTFChars(e,r,0), *op=(*e)->GetStringUTFChars(e,o,0);
    char *out=spike_run(root,op,(size_t)n);
    jstring result=(*e)->NewStringUTF(e,out);
    spike_free(out);
    (*e)->ReleaseStringUTFChars(e,r,root); (*e)->ReleaseStringUTFChars(e,o,op);
    return result;
}
