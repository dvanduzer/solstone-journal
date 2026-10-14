// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#include <stdint.h>
uint64_t backup_probe_start(const char *json, int32_t *code);
char *backup_probe_wait(uint64_t id);
int32_t backup_probe_cancel(uint64_t id);
int32_t backup_probe_free(uint64_t id);
void backup_probe_json_free(char *json);
uint64_t backup_probe_footprint(void);
int32_t backup_probe_set_fixture_metadata(const char *path);
int32_t backup_probe_check_fixture_metadata(const char *path);
uint64_t backup_probe_device(const char *path);
