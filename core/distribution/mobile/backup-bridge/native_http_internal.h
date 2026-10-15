// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#ifndef SOLSTONE_NATIVE_HTTP_INTERNAL_H
#define SOLSTONE_NATIVE_HTTP_INTERNAL_H
#include <stdint.h>

typedef struct sb_http_call sb_http_call;
sb_http_call *sb_http_create(const void *metadata, uint32_t metadata_len,
                           const void *body, uint32_t body_len, uint32_t response_limit);
void sb_http_start(sb_http_call *call);
void sb_http_cancel(sb_http_call *call);
int32_t sb_http_wait(sb_http_call *call);
const void *sb_http_metadata(sb_http_call *call, uint32_t *length);
const void *sb_http_body(sb_http_call *call, uint32_t *length);
void sb_http_release(sb_http_call *call);
#endif
