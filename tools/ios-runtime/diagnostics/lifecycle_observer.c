// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#include <stdint.h>
uint64_t ios_observer_load_acquire(const uint64_t *pointer) {
  return __atomic_load_n(pointer, __ATOMIC_ACQUIRE);
}
