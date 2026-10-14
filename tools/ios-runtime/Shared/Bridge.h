// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#include <stdbool.h>

char *journal_runtime(const char *input);
void journal_runtime_free(char *result);
char *runtime_resources(void);
void runtime_resources_free(char *result);

typedef char *(*journal_ios_transport_fn)(const char *request_json);

bool journal_ios_verifier_admit(void);
bool journal_ios_verifier_register_callback(journal_ios_transport_fn transport);
void journal_ios_verifier_cancel(void);
bool journal_ios_verifier_drain_ready(void);
