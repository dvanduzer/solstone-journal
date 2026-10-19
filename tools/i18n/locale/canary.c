// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#include <locale.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
int main(int argc, char **argv) {
    if (argc != 3) return 2;
    const char *resolved = setlocale(LC_ALL, "");
    if (!resolved || strcmp(resolved, argv[1]) != 0) {
        fprintf(stderr, "locale activation refused: requested=%s resolved=%s\n", argv[1], resolved ? resolved : "NULL");
        return 1;
    }
    struct tm sample = { .tm_year = 126, .tm_mon = 9, .tm_mday = 19 };
    char month[256] = {0};
    if (!strftime(month, sizeof(month), "%B", &sample) || strcmp(month, argv[2]) != 0) {
        fprintf(stderr, "locale canary mismatch: expected=%s actual=%s\n", argv[2], month);
        return 1;
    }
    printf("requested=%s resolved=%s month=%s decimal=%s\n", argv[1], resolved, month, localeconv()->decimal_point);
    return 0;
}
