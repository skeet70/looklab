# SPDX-License-Identifier: AGPL-3.0-or-later
#
# The library has no allocator. `make check` builds it with gcc and clang under
# -std=c99 -Wall -Wextra -Werror -pedantic, builds an ASan+UBSan binary for the
# malformed-card runs, and links a binary whose allocator entry points abort.

CC ?= cc
CLANG ?= clang
OUTDIR ?= build
CPPFLAGS ?=
CFLAGS ?= -O2
LDFLAGS ?=
STRICT = -std=c99 -Wall -Wextra -Werror -pedantic
LDLIBS = -lm

SOURCES = src/llk_card.c src/llk_fit.c src/llk_sha256.c src/llk_main.c
HEADERS = src/llk.h src/llk_sha256.h
ALLOC_GUARD = tests/alloc_guard.c
WRAPS = -Wl,--wrap=malloc -Wl,--wrap=calloc -Wl,--wrap=realloc -Wl,--wrap=free

all: $(OUTDIR)/llk-translate

$(OUTDIR)/llk-translate: $(SOURCES) $(HEADERS)
	@mkdir -p $(OUTDIR)
	$(CC) $(CPPFLAGS) $(STRICT) $(CFLAGS) -o $@ $(SOURCES) $(LDFLAGS) $(LDLIBS)

$(OUTDIR)/llk-translate-gcc: $(SOURCES) $(HEADERS)
	@mkdir -p $(OUTDIR)
	gcc $(CPPFLAGS) $(STRICT) $(CFLAGS) -o $@ $(SOURCES) $(LDFLAGS) $(LDLIBS)

$(OUTDIR)/llk-translate-clang: $(SOURCES) $(HEADERS)
	@mkdir -p $(OUTDIR)
	$(CLANG) $(CPPFLAGS) $(STRICT) $(CFLAGS) -o $@ $(SOURCES) $(LDFLAGS) $(LDLIBS)

$(OUTDIR)/llk-translate-asan: $(SOURCES) $(HEADERS)
	@mkdir -p $(OUTDIR)
	$(CLANG) $(CPPFLAGS) $(STRICT) -O1 -g -fno-omit-frame-pointer \
		-fsanitize=address,undefined -o $@ $(SOURCES) $(LDFLAGS) $(LDLIBS)

$(OUTDIR)/llk-translate-guard: $(SOURCES) $(HEADERS) $(ALLOC_GUARD)
	@mkdir -p $(OUTDIR)
	gcc $(CPPFLAGS) $(STRICT) $(CFLAGS) -o $@ $(SOURCES) $(ALLOC_GUARD) $(WRAPS) \
		$(LDFLAGS) $(LDLIBS)

check: $(OUTDIR)/llk-translate-gcc $(OUTDIR)/llk-translate-clang \
	$(OUTDIR)/llk-translate-asan $(OUTDIR)/llk-translate-guard
	@if nm -u $(OUTDIR)/llk-translate-gcc | grep -Eq ' U (malloc|calloc|realloc|free)$$'; then \
		echo "error: allocator symbol referenced"; exit 1; fi
	@echo "check: strict builds, no allocator symbols"

clean:
	rm -rf $(OUTDIR)

.PHONY: all check clean
