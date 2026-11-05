/* SPDX-License-Identifier: AGPL-3.0-or-later */
/*
 * Firmware links the library and supplies its own read callback; this program
 * supplies one over a stdio file.
 */
#include "llk.h"

#include <stdio.h>
#include <string.h>

static int file_read(void *ctx, uint32_t offset, uint8_t *dst, uint32_t len)
{
    FILE *file = ctx;

    if (fseek(file, (long)offset, SEEK_SET) != 0) {
        return -1;
    }
    return fread(dst, 1u, len, file) == len ? 0 : -1;
}

static void print_json_string(const char *text)
{
    const unsigned char *p = (const unsigned char *)text;

    while (*p != 0u) {
        if (*p == '"' || *p == '\\') {
            putchar('\\');
            putchar((int)*p);
        } else if (*p < 0x20u) {
            printf("\\u%04x", (unsigned)*p);
        } else {
            putchar((int)*p);
        }
        p++;
    }
}

static void print_meta(const llk_card *card)
{
    uint32_t i;

    printf("{\"content_id\":\"");
    for (i = 0; i < 32u; i++) {
        printf("%02x", (unsigned)card->content_id[i]);
    }
    printf("\",\"slug\":\"");
    print_json_string(card->slug);
    printf("\",\"version\":\"");
    print_json_string(card->version);
    printf("\",\"name\":\"");
    print_json_string(card->name);
    printf("\",\"short_name\":\"");
    print_json_string(card->short_name);
    printf("\",\"sections\":%u,\"has_grid\":%s}", (unsigned)card->section_count,
           card->has_grid ? "true" : "false");
}

static void print_translation(const llk_card *card, const llk_translation *t)
{
    uint32_t j;
    int ch;

    printf("{\"content_id\":\"");
    for (j = 0; j < 32u; j++) {
        printf("%02x", (unsigned)card->content_id[j]);
    }
    printf("\",\"matrix\":[");
    for (ch = 0; ch < 3; ch++) {
        printf("%s[%.9g,%.9g,%.9g]", ch == 0 ? "" : ",", (double)t->matrix[ch][0],
               (double)t->matrix[ch][1], (double)t->matrix[ch][2]);
    }
    printf("],\"curve\":{\"points\":%u,\"ev\":[", (unsigned)t->curve_points);
    for (j = 0; j < t->curve_points; j++) {
        printf("%s%.9g", j == 0u ? "" : ",", (double)t->curve_ev[j]);
    }
    printf("],\"input\":[");
    for (j = 0; j < t->curve_points; j++) {
        printf("%s%.9g", j == 0u ? "" : ",", (double)t->curve_input[j]);
    }
    printf("],\"r\":[");
    for (j = 0; j < t->curve_points; j++) {
        printf("%s%.9g", j == 0u ? "" : ",", (double)t->curve[0][j]);
    }
    printf("],\"g\":[");
    for (j = 0; j < t->curve_points; j++) {
        printf("%s%.9g", j == 0u ? "" : ",", (double)t->curve[1][j]);
    }
    printf("],\"b\":[");
    for (j = 0; j < t->curve_points; j++) {
        printf("%s%.9g", j == 0u ? "" : ",", (double)t->curve[2][j]);
    }
    printf("]},\"fit\":{\"probes\":%u,\"supported\":%u,\"fitted\":%u,"
           "\"lab_rms\":%.9g,\"lab_max\":%.9g}}",
           (unsigned)t->probes, (unsigned)t->supported, (unsigned)t->fitted, t->lab_rms,
           t->lab_max);
}

int main(int argc, char **argv)
{
    const char *path = NULL;
    int inspect = 0;
    int arg;
    llk_reader reader;
    llk_card card;
    llk_status status;
    FILE *file;

    for (arg = 1; arg < argc; arg++) {
        if (strcmp(argv[arg], "--inspect") == 0) {
            inspect = 1;
        } else if (path == NULL) {
            path = argv[arg];
        } else {
            fprintf(stderr, "usage: %s [--inspect] <card.llk>\n", argv[0]);
            return 1;
        }
    }
    if (path == NULL) {
        fprintf(stderr, "usage: %s [--inspect] <card.llk>\n", argv[0]);
        return 1;
    }
    file = fopen(path, "rb");
    if (file == NULL) {
        fprintf(stderr, "%s: cannot open\n", path);
        return 1;
    }
    reader.read = file_read;
    reader.ctx = file;
    status = llk_card_open(&reader, &card);
    if (status == LLK_OK && !inspect) {
        llk_translation translation;

        status = llk_translate(&card, &translation);
        if (status == LLK_OK) {
            print_translation(&card, &translation);
            printf("\n");
        }
    } else if (status == LLK_OK) {
        print_meta(&card);
        printf("\n");
    }
    fclose(file);
    if (status != LLK_OK) {
        fprintf(stderr, "%s: rejected: %s\n", path, llk_status_str(status));
        return 2;
    }
    return 0;
}
