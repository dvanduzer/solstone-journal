// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#include <jni.h>
#include <stddef.h>
extern char *journal_run(const char *, const char *, size_t);
extern void journal_free(char *);
JNIEXPORT jstring JNICALL Java_app_solstone_development_journal_Native_run(JNIEnv *e, jclass c, jstring r, jstring o, jint n) {
    (void)c;
    if (!r || !o) {
        jclass argument_error=(*e)->FindClass(e,"java/lang/IllegalArgumentException");
        if (argument_error) (*e)->ThrowNew(e,argument_error,"root and operation are required");
        return NULL;
    }
    const char *root=(*e)->GetStringUTFChars(e,r,0);
    if (!root) return NULL;
    const char *op=(*e)->GetStringUTFChars(e,o,0);
    if (!op) {
        (*e)->ReleaseStringUTFChars(e,r,root);
        return NULL;
    }
    char *out=journal_run(root,op,(size_t)n);
    jstring result=(*e)->NewStringUTF(e,out);
    journal_free(out);
    (*e)->ReleaseStringUTFChars(e,r,root); (*e)->ReleaseStringUTFChars(e,o,op);
    return result;
}
