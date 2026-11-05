/* SPDX-License-Identifier: AGPL-3.0-or-later */
/*
 * looklab card translator, portable C99, no allocator.
 *
 * Reads a firmware card (docs/04 section 8) through a caller-provided read
 * callback and fits a 3x3 matrix followed by a per-channel tone curve, the
 * shape a hacked camera firmware can program. The fit uses NEUTRAL for tone
 * and FIT for colour (docs/04 section 8.5).
 */
#ifndef LLK_H
#define LLK_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define LLK_HEADER_LEN 64u
#define LLK_FORMAT_VERSION 1u
#define LLK_NEUTRAL_POINTS 257u
#define LLK_FIT_RECORD_LEN 40u
#define LLK_MAX_SECTIONS 64u
#define LLK_MAX_META_BYTES 4096u
#define LLK_READ_CHUNK 1024u

/* Curve knots; the default matches the 28-point tone curve of x70view. */
#ifndef LLK_CURVE_POINTS
#define LLK_CURVE_POINTS 28
#endif

#if LLK_CURVE_POINTS < 2
#error "LLK_CURVE_POINTS must be at least 2"
#endif
#if LLK_CURVE_POINTS > LLK_NEUTRAL_POINTS
#error "LLK_CURVE_POINTS must not exceed LLK_NEUTRAL_POINTS"
#endif

/* Header field values of a v1 card. */
enum {
    LLK_SECTION_META = 1,
    LLK_SECTION_SEMANTICS = 2,
    LLK_SECTION_NEUTRAL = 3,
    LLK_SECTION_FIT = 4,
    LLK_SECTION_GRID = 5
};

typedef enum {
    LLK_OK = 0,
    LLK_ERR_IO,
    LLK_ERR_MAGIC,
    LLK_ERR_VERSION,
    LLK_ERR_HEADER,         /* header length, reserved bytes or file length */
    LLK_ERR_LAYOUT,         /* section table or section placement */
    LLK_ERR_CRC,            /* header or section CRC-32 */
    LLK_ERR_CONTENT_ID,
    LLK_ERR_MISSING_SECTION,
    LLK_ERR_SECTION,
    LLK_ERR_NOT_FINITE,
    LLK_ERR_META,
    LLK_ERR_NO_PROBES,
    LLK_ERR_DEGENERATE      /* the probe set cannot determine the matrix */
} llk_status;

/*
 * Returns 0 only if it filled all len bytes at offset. Reads are absolute and
 * may revisit earlier offsets.
 */
typedef int (*llk_read_fn)(void *ctx, uint32_t offset, uint8_t *dst, uint32_t len);

typedef struct {
    llk_read_fn read;
    void *ctx;
} llk_reader;

typedef struct {
    uint8_t content_id[32];
    char slug[64];
    char version[32];
    char name[64];
    char short_name[28];
    uint16_t section_count;
    uint8_t has_grid;
    uint8_t reserved;
    /* NEUTRAL: output for a neutral input at s = -12 + 18*i/(n-1), linear
     * Rec.2020, pre-clip. */
    float neutral[LLK_NEUTRAL_POINTS][3];
    /* FIT section location; llk_translate streams it. */
    uint32_t fit_offset;
    uint32_t fit_length;
    uint32_t fit_probes;
    llk_reader reader;
} llk_card;

typedef struct {
    /* Row-normalized: each row sums to 1 within fit error. The curve absorbs each
     * original row's neutral gain, so the product reproduces the card. */
    float matrix[3][3];
    uint32_t curve_points;
    /* EV relative to mid gray 0.18: input x = 0.18 * 2^ev. */
    float curve_ev[LLK_CURVE_POINTS];
    /* The same knots as linear input. */
    float curve_input[LLK_CURVE_POINTS];
    /* Linear Rec.2020 output per channel at each knot. */
    float curve[3][LLK_CURVE_POINTS];
    uint32_t probes;
    uint32_t supported; /* flag bit 0 set */
    uint32_t fitted;    /* records the model could represent */
    /* Weighted CIELAB dE over the fitted probes. */
    double lab_rms;
    double lab_max;
} llk_translation;

/* Validates and loads the card. On any error the card is left untouched. */
llk_status llk_card_open(const llk_reader *reader, llk_card *card);

/* Fits matrix and curve. Re-reads FIT through card->reader. */
llk_status llk_translate(const llk_card *card, llk_translation *out);

/* out = curve(matrix * in), the model llk_translate fits. */
void llk_apply(const llk_translation *t, const float in[3], float out[3]);

/* One fitted channel curve at linear input x; x <= 0 clamps to the first knot. */
double llk_curve_eval(const llk_translation *t, int channel, double x);

const char *llk_status_str(llk_status status);

#ifdef __cplusplus
}
#endif

#endif /* LLK_H */
