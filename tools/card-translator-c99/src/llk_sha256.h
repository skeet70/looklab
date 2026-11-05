/* SPDX-License-Identifier: AGPL-3.0-or-later */
/* SHA-256 (FIPS 180-4), streaming, no allocation. */
#ifndef LLK_SHA256_H
#define LLK_SHA256_H

#include <stddef.h>
#include <stdint.h>

typedef struct {
    uint32_t state[8];
    uint64_t total;
    uint8_t block[64];
    size_t used;
} llk_sha256;

void llk_sha256_init(llk_sha256 *ctx);
void llk_sha256_update(llk_sha256 *ctx, const uint8_t *data, size_t len);
void llk_sha256_final(llk_sha256 *ctx, uint8_t digest[32]);

#endif /* LLK_SHA256_H */
