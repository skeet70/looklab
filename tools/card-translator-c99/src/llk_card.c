/* SPDX-License-Identifier: AGPL-3.0-or-later */
#include "llk.h"

#include "llk_sha256.h"

#include <limits.h>
#include <math.h>
#include <string.h>

typedef char llk_require_8_bit_bytes[(CHAR_BIT == 8) ? 1 : -1];
typedef char llk_require_4_byte_float[(sizeof(float) == 4) ? 1 : -1];

typedef struct {
    uint32_t kind;
    uint32_t offset;
    uint32_t length;
    uint32_t crc;
} section_entry;

static uint16_t rd16(const uint8_t *p)
{
    return (uint16_t)((uint16_t)p[0] | (uint16_t)((uint16_t)p[1] << 8));
}

static uint32_t rd32(const uint8_t *p)
{
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) |
           ((uint32_t)p[3] << 24);
}

static float rdf32(const uint8_t *p)
{
    uint32_t bits = rd32(p);
    float value;

    memcpy(&value, &bits, sizeof value);
    return value;
}

static uint32_t crc32_update(uint32_t crc, const uint8_t *buf, uint32_t len)
{
    uint32_t i;
    uint32_t k;

    crc = ~crc;
    for (i = 0; i < len; i++) {
        crc ^= buf[i];
        for (k = 0; k < 8u; k++) {
            crc = (crc >> 1) ^ (0xedb88320u & (0u - (crc & 1u)));
        }
    }
    return ~crc;
}

static llk_status read_exact(const llk_reader *reader, uint32_t offset, uint8_t *dst,
                             uint32_t len)
{
    if (len == 0u) {
        return LLK_OK;
    }
    if (reader->read(reader->ctx, offset, dst, len) != 0) {
        return LLK_ERR_IO;
    }
    return LLK_OK;
}

static llk_status section_crc(const llk_reader *reader, const section_entry *entry)
{
    uint8_t chunk[LLK_READ_CHUNK];
    uint32_t done = 0;
    uint32_t crc = 0;

    while (done < entry->length) {
        uint32_t n = entry->length - done;
        llk_status status;

        if (n > (uint32_t)sizeof chunk) {
            n = (uint32_t)sizeof chunk;
        }
        status = read_exact(reader, entry->offset + done, chunk, n);
        if (status != LLK_OK) {
            return status;
        }
        crc = crc32_update(crc, chunk, n);
        done += n;
    }
    if (crc != entry->crc) {
        return LLK_ERR_CRC;
    }
    return LLK_OK;
}

static int ascii_field(const uint8_t *p, uint32_t len, uint32_t max_len)
{
    uint32_t i;

    if (len == 0u || len > max_len) {
        return 0;
    }
    for (i = 0; i < len; i++) {
        if (p[i] < 0x20u || p[i] > 0x7eu) {
            return 0;
        }
    }
    return 1;
}

static int valid_slug(const uint8_t *p, uint32_t len)
{
    uint32_t i;

    if (len == 0u || len > 63u) {
        return 0;
    }
    for (i = 0; i < len; i++) {
        uint8_t c = p[i];

        if (!((c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '.' || c == '_' ||
              c == '-')) {
            return 0;
        }
    }
    return 1;
}

static int valid_version(const uint8_t *p, uint32_t len)
{
    uint32_t i;

    if (len == 0u || len > 31u) {
        return 0;
    }
    for (i = 0; i < len; i++) {
        uint8_t c = p[i];

        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
              c == '.' || c == '+' || c == '-')) {
            return 0;
        }
    }
    return 1;
}

static int utf8_valid(const uint8_t *p, uint32_t len)
{
    uint32_t i = 0;

    while (i < len) {
        uint32_t cp;
        uint32_t need;
        uint32_t k;
        uint8_t c = p[i];

        if (c < 0x80u) {
            if (c == 0u) {
                return 0;
            }
            i++;
            continue;
        } else if ((c & 0xe0u) == 0xc0u) {
            cp = (uint32_t)(c & 0x1fu);
            need = 1;
        } else if ((c & 0xf0u) == 0xe0u) {
            cp = (uint32_t)(c & 0x0fu);
            need = 2;
        } else if ((c & 0xf8u) == 0xf0u) {
            cp = (uint32_t)(c & 0x07u);
            need = 3;
        } else {
            return 0;
        }
        if (len - i <= need) {
            return 0;
        }
        for (k = 1; k <= need; k++) {
            if ((p[i + k] & 0xc0u) != 0x80u) {
                return 0;
            }
            cp = (cp << 6) | (uint32_t)(p[i + k] & 0x3fu);
        }
        if ((need == 1 && cp < 0x80u) || (need == 2 && cp < 0x800u) ||
            (need == 3 && cp < 0x10000u) || cp > 0x10ffffu ||
            (cp >= 0xd800u && cp <= 0xdfffu)) {
            return 0;
        }
        i += need + 1;
    }
    return 1;
}

static llk_status parse_meta(const uint8_t *p, uint32_t len, llk_card *card)
{
    uint32_t pos = 0;
    int have_slug = 0;
    int have_version = 0;
    int have_name = 0;
    int have_short = 0;

    while (pos < len) {
        uint8_t key;
        uint32_t value_len;
        const uint8_t *value;

        if (len - pos < 3u) {
            return LLK_ERR_META;
        }
        key = p[pos];
        value_len = rd16(p + pos + 1);
        pos += 3u;
        if (value_len > len - pos) {
            return LLK_ERR_META;
        }
        value = p + pos;
        pos += value_len;

        switch (key) {
        case 1:
            if (!valid_slug(value, value_len)) {
                return LLK_ERR_META;
            }
            memcpy(card->slug, value, value_len);
            card->slug[value_len] = '\0';
            have_slug = 1;
            break;
        case 2:
            if (!valid_version(value, value_len)) {
                return LLK_ERR_META;
            }
            memcpy(card->version, value, value_len);
            card->version[value_len] = '\0';
            have_version = 1;
            break;
        case 3:
            if (value_len == 0u || value_len > 63u || !utf8_valid(value, value_len)) {
                return LLK_ERR_META;
            }
            memcpy(card->name, value, value_len);
            card->name[value_len] = '\0';
            have_name = 1;
            break;
        case 4:
            if (!ascii_field(value, value_len, 27u)) {
                return LLK_ERR_META;
            }
            memcpy(card->short_name, value, value_len);
            card->short_name[value_len] = '\0';
            have_short = 1;
            break;
        default:
            break;
        }
    }
    if (!have_slug || !have_version || !have_name || !have_short) {
        return LLK_ERR_META;
    }
    return LLK_OK;
}

static llk_status validate_grid(const llk_reader *reader, const section_entry *grid)
{
    uint8_t chunk[LLK_READ_CHUNK];
    uint8_t buffer[4];
    uint32_t side;
    uint32_t crc = 0;
    uint64_t nodes;
    uint64_t expected;
    uint64_t done = 0;
    llk_status status;

    if (grid->length < 4u) {
        return LLK_ERR_SECTION;
    }
    status = read_exact(reader, grid->offset, buffer, sizeof buffer);
    if (status != LLK_OK) {
        return status;
    }
    crc = crc32_update(crc, buffer, sizeof buffer);
    side = rd32(buffer);
    if (side != 33u && side != 65u) {
        return LLK_ERR_SECTION;
    }
    nodes = (uint64_t)side * (uint64_t)side * (uint64_t)side;
    expected = 4u + nodes * 6u;
    if (expected != (uint64_t)grid->length) {
        return LLK_ERR_SECTION;
    }
    while (done < expected - 4u) {
        uint32_t n = (uint32_t)(expected - 4u - done);
        uint32_t i;

        if (n > (uint32_t)sizeof chunk) {
            n = (uint32_t)sizeof chunk;
        }
        status = read_exact(reader, grid->offset + 4u + (uint32_t)done, chunk, n);
        if (status != LLK_OK) {
            return status;
        }
        crc = crc32_update(crc, chunk, n);
        for (i = 0; i < n; i += 2u) {
            uint32_t bits = (uint32_t)chunk[i] | ((uint32_t)chunk[i + 1u] << 8);

            if ((bits & 0x7c00u) == 0x7c00u) {
                return LLK_ERR_NOT_FINITE;
            }
        }
        done += n;
    }
    if (crc != grid->crc) {
        return LLK_ERR_CRC;
    }
    return LLK_OK;
}

const char *llk_status_str(llk_status status)
{
    switch (status) {
    case LLK_OK:
        return "ok";
    case LLK_ERR_IO:
        return "read callback failed";
    case LLK_ERR_MAGIC:
        return "bad magic";
    case LLK_ERR_VERSION:
        return "unsupported version";
    case LLK_ERR_HEADER:
        return "bad header";
    case LLK_ERR_LAYOUT:
        return "bad layout";
    case LLK_ERR_CRC:
        return "crc mismatch";
    case LLK_ERR_CONTENT_ID:
        return "content id mismatch";
    case LLK_ERR_MISSING_SECTION:
        return "missing section";
    case LLK_ERR_SECTION:
        return "malformed section";
    case LLK_ERR_NOT_FINITE:
        return "non-finite value";
    case LLK_ERR_META:
        return "malformed meta";
    case LLK_ERR_NO_PROBES:
        return "no supported probes";
    case LLK_ERR_DEGENERATE:
        return "degenerate probes";
    }
    return "unknown error";
}

static llk_status validate_fit(const llk_reader *reader, const section_entry *fit,
                             uint32_t probes)
{
    uint8_t buffer[LLK_FIT_RECORD_LEN * 8u];
    uint32_t index = 0;

    while (index < probes) {
        uint32_t batch = probes - index;
        uint32_t k;

        if (batch > 8u) {
            batch = 8u;
        }
        if (read_exact(reader, fit->offset + 4u + index * LLK_FIT_RECORD_LEN, buffer,
                       batch * LLK_FIT_RECORD_LEN) != LLK_OK) {
            return LLK_ERR_IO;
        }
        for (k = 0; k < batch; k++) {
            const uint8_t *p = buffer + k * LLK_FIT_RECORD_LEN;
            float sigma = rdf32(p + 24u);
            float weight = rdf32(p + 28u);
            uint32_t ch;

            for (ch = 0; ch < 3u; ch++) {
                if (!isfinite(rdf32(p + ch * 4u)) ||
                    !isfinite(rdf32(p + 12u + ch * 4u))) {
                    return LLK_ERR_NOT_FINITE;
                }
            }
            if (sigma != sigma || (isinf(sigma) && sigma < 0.0f) || !isfinite(weight)) {
                return LLK_ERR_NOT_FINITE;
            }
            if (sigma < 0.0f || weight < 0.0f) {
                return LLK_ERR_SECTION;
            }
        }
        index += batch;
    }
    return LLK_OK;
}

llk_status llk_card_open(const llk_reader *reader, llk_card *card)
{
    uint8_t header[LLK_HEADER_LEN];
    uint8_t table[LLK_MAX_SECTIONS * 16u];
    section_entry entries[LLK_MAX_SECTIONS];
    llk_card loaded;
    uint32_t file_len;
    uint32_t count;
    uint32_t table_len;
    uint32_t table_end;
    uint32_t i;
    uint32_t j;
    uint32_t header_crc;
    uint32_t crc;
    const uint8_t zero4[4] = {0u, 0u, 0u, 0u};
    const section_entry *meta = NULL;
    const section_entry *semantics = NULL;
    const section_entry *neutral = NULL;
    const section_entry *fit = NULL;
    const section_entry *grid = NULL;
    llk_status status;

    if (reader == NULL || card == NULL || reader->read == NULL) {
        return LLK_ERR_IO;
    }
    memset(&loaded, 0, sizeof loaded);
    loaded.reader = *reader;

    status = read_exact(reader, 0u, header, LLK_HEADER_LEN);
    if (status != LLK_OK) {
        return status;
    }
    if (memcmp(header, "LLCF", 4u) != 0) {
        return LLK_ERR_MAGIC;
    }
    if (rd16(header + 4) != LLK_FORMAT_VERSION) {
        return LLK_ERR_VERSION;
    }
    if (rd16(header + 6) != LLK_HEADER_LEN) {
        return LLK_ERR_HEADER;
    }
    file_len = rd32(header + 8);
    count = rd16(header + 12);
    if (rd16(header + 14) != 0u || rd32(header + 8) < LLK_HEADER_LEN) {
        return LLK_ERR_HEADER;
    }
    for (i = 0; i < 12u; i++) {
        if (header[0x34u + i] != 0u) {
            return LLK_ERR_HEADER;
        }
    }
    if (count > LLK_MAX_SECTIONS) {
        return LLK_ERR_LAYOUT;
    }
    table_len = count * 16u;
    table_end = LLK_HEADER_LEN + table_len;
    if (table_end > file_len) {
        return LLK_ERR_LAYOUT;
    }
    status = read_exact(reader, LLK_HEADER_LEN, table, table_len);
    if (status != LLK_OK) {
        return status;
    }

    crc = crc32_update(0u, header, 16u);
    crc = crc32_update(crc, zero4, 4u);
    crc = crc32_update(crc, header + 20, 44u);
    crc = crc32_update(crc, table, table_len);
    header_crc = rd32(header + 16);
    if (crc != header_crc) {
        return LLK_ERR_CRC;
    }

    for (i = 0; i < count; i++) {
        const uint8_t *p = table + i * 16u;

        entries[i].kind = rd32(p);
        entries[i].offset = rd32(p + 4);
        entries[i].length = rd32(p + 8);
        entries[i].crc = rd32(p + 12);
        if (i > 0u && entries[i].kind <= entries[i - 1u].kind) {
            return LLK_ERR_LAYOUT;
        }
        if ((entries[i].offset & 3u) != 0u || entries[i].offset < table_end) {
            return LLK_ERR_LAYOUT;
        }
        if ((uint64_t)entries[i].offset + (uint64_t)entries[i].length > (uint64_t)file_len) {
            return LLK_ERR_LAYOUT;
        }
    }
    for (i = 0; i < count; i++) {
        for (j = i + 1u; j < count; j++) {
            uint64_t a0 = entries[i].offset;
            uint64_t a1 = a0 + entries[i].length;
            uint64_t b0 = entries[j].offset;
            uint64_t b1 = b0 + entries[j].length;

            if (a0 < b1 && b0 < a1) {
                return LLK_ERR_LAYOUT;
            }
        }
    }

    for (i = 0; i < count; i++) {
        switch (entries[i].kind) {
        case LLK_SECTION_META:
            meta = &entries[i];
            break;
        case LLK_SECTION_SEMANTICS:
            semantics = &entries[i];
            break;
        case LLK_SECTION_NEUTRAL:
            neutral = &entries[i];
            break;
        case LLK_SECTION_FIT:
            fit = &entries[i];
            break;
        case LLK_SECTION_GRID:
            grid = &entries[i];
            loaded.has_grid = 1u;
            break;
        default:
            status = section_crc(reader, &entries[i]);
            if (status != LLK_OK) {
                return status;
            }
            break;
        }
    }
    if (meta == NULL || semantics == NULL || neutral == NULL || fit == NULL) {
        return LLK_ERR_MISSING_SECTION;
    }

    {
        uint8_t chunk[LLK_READ_CHUNK];
        uint8_t digest[32];
        uint32_t pos = table_end;
        llk_sha256 sha;

        llk_sha256_init(&sha);
        while (pos < file_len) {
            uint32_t n = file_len - pos;

            if (n > (uint32_t)sizeof chunk) {
                n = (uint32_t)sizeof chunk;
            }
            status = read_exact(reader, pos, chunk, n);
            if (status != LLK_OK) {
                return status;
            }
            llk_sha256_update(&sha, chunk, n);
            pos += n;
        }
        llk_sha256_final(&sha, digest);
        if (memcmp(digest, header + 0x14u, sizeof digest) != 0) {
            return LLK_ERR_CONTENT_ID;
        }
        memcpy(loaded.content_id, header + 0x14u, sizeof loaded.content_id);
    }

    status = section_crc(reader, meta);
    if (status != LLK_OK) {
        return status;
    }
    if (meta->length > LLK_MAX_META_BYTES) {
        return LLK_ERR_META;
    }
    {
        uint8_t buffer[LLK_MAX_META_BYTES];

        status = read_exact(reader, meta->offset, buffer, meta->length);
        if (status != LLK_OK) {
            return status;
        }
        status = parse_meta(buffer, meta->length, &loaded);
        if (status != LLK_OK) {
            return status;
        }
    }

    status = section_crc(reader, semantics);
    if (status != LLK_OK) {
        return status;
    }
    if (semantics->length != 12u) {
        return LLK_ERR_SECTION;
    }
    {
        uint8_t buffer[12];

        status = read_exact(reader, semantics->offset, buffer, sizeof buffer);
        if (status != LLK_OK) {
            return status;
        }
        for (i = 0; i < 5u; i++) {
            if (rd16(buffer + i * 2u) != 1u) {
                return LLK_ERR_SECTION;
            }
        }
        if (rd16(buffer + 10) != 0u) {
            return LLK_ERR_SECTION;
        }
    }

    status = section_crc(reader, neutral);
    if (status != LLK_OK) {
        return status;
    }
    if (neutral->length != 4u + LLK_NEUTRAL_POINTS * 12u) {
        return LLK_ERR_SECTION;
    }
    {
        uint8_t buffer[4];
        uint32_t n;

        status = read_exact(reader, neutral->offset, buffer, sizeof buffer);
        if (status != LLK_OK) {
            return status;
        }
        n = rd32(buffer);
        if (n != LLK_NEUTRAL_POINTS) {
            return LLK_ERR_SECTION;
        }
    }
    {
        uint8_t chunk[64u * 12u];
        uint32_t index = 0;

        while (index < LLK_NEUTRAL_POINTS) {
            uint32_t points = LLK_NEUTRAL_POINTS - index;
            uint32_t k;
            uint32_t ch;

            if (points > 64u) {
                points = 64u;
            }
            status = read_exact(reader, neutral->offset + 4u + index * 12u, chunk,
                                points * 12u);
            if (status != LLK_OK) {
                return status;
            }
            for (k = 0; k < points; k++) {
                for (ch = 0; ch < 3u; ch++) {
                    float value = rdf32(chunk + (k * 3u + ch) * 4u);

                    if (!isfinite(value)) {
                        return LLK_ERR_NOT_FINITE;
                    }
                    loaded.neutral[index + k][ch] = value;
                }
            }
            index += points;
        }
    }

    status = section_crc(reader, fit);
    if (status != LLK_OK) {
        return status;
    }
    if (fit->length < 4u) {
        return LLK_ERR_SECTION;
    }
    {
        uint8_t buffer[4];

        status = read_exact(reader, fit->offset, buffer, sizeof buffer);
        if (status != LLK_OK) {
            return status;
        }
        loaded.fit_probes = rd32(buffer);
    }
    if (loaded.fit_probes == 0u ||
        (uint64_t)4u + (uint64_t)loaded.fit_probes * LLK_FIT_RECORD_LEN !=
            (uint64_t)fit->length) {
        return LLK_ERR_SECTION;
    }
    status = validate_fit(reader, fit, loaded.fit_probes);
    if (status != LLK_OK) {
        return status;
    }
    loaded.fit_offset = fit->offset;
    loaded.fit_length = fit->length;
    loaded.section_count = (uint16_t)count;

    if (grid != NULL) {
        status = validate_grid(reader, grid);
        if (status != LLK_OK) {
            return status;
        }
    }

    *card = loaded;
    return LLK_OK;
}
