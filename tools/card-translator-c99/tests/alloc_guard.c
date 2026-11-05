/* SPDX-License-Identifier: AGPL-3.0-or-later */
/* Link-time proof that the translator never reaches the allocator. */
#include <stddef.h>
#include <stdlib.h>

void *__wrap_malloc(size_t size)
{
    (void)size;
    abort();
}

void *__wrap_calloc(size_t count, size_t size)
{
    (void)count;
    (void)size;
    abort();
}

void *__wrap_realloc(void *ptr, size_t size)
{
    (void)ptr;
    (void)size;
    abort();
}

void __wrap_free(void *ptr)
{
    (void)ptr;
    abort();
}
