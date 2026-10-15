// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#include <stdint.h>
uint64_t backup_probe_start(const char *json, int32_t *code);
char *backup_probe_wait(uint64_t id, int32_t *wait_code);
int32_t backup_probe_cancel(uint64_t id);
int32_t backup_probe_free(uint64_t id);
void backup_probe_json_free(char *json);
uint64_t backup_probe_footprint(void);
int32_t backup_probe_set_fixture_metadata(const char *path);
int32_t backup_probe_check_fixture_metadata(const char *path);
uint64_t backup_probe_device(const char *path);
int32_t backup_s3_prepare_roots(const char *owner, const char *input, int32_t fresh);
char *backup_s3_load_fixture(uint32_t *length, int32_t *code);
void backup_s3_fixture_free(char *bytes);
void *backup_s3_observer_create(const char *destination, int32_t operation, int32_t require_write);
char *backup_s3_observer_wait(void *observer, uint32_t milliseconds);
int32_t backup_s3_observer_free(void *observer);
char *backup_s3_read_owner(const char *destination);
void *backup_s3_alternate_root_create(const char *name);
int32_t backup_s3_alternate_root_check(void *root, const char *path);
int32_t backup_s3_alternate_root_free(void *root);
