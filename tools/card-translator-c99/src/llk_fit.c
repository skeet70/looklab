/* SPDX-License-Identifier: AGPL-3.0-or-later */
/*
 * Tone from NEUTRAL, matrix from FIT (docs/04 section 8.5).
 *
 * Model: out = curve(matrix * in), the curve per channel over EV relative to
 * mid gray 0.18. The curve is read off NEUTRAL; the matrix is a weighted least
 * squares fit on the supported FIT probes with the tone inverse applied, then
 * refined to minimise a weighted CIELAB error. With an exact 3x3-plus-curve
 * source both stages reproduce the parameters.
 */
#include "llk.h"

#include <math.h>
#include <string.h>

#define LLK_MID_GRAY 0.18
#define LLK_EV_MIN (-12.0)
#define LLK_EV_MAX 6.0
#define LLK_GN_ITERATIONS 8
#define LLK_GN_ATTEMPTS 5

typedef struct {
    float in[3];
    float out[3];
    float sigma;
    float weight;
    uint32_t flags;
    uint32_t probe;
} fit_record;

typedef int (*record_fn)(void *ctx, const fit_record *record);

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

/* D65, Y = 1 at RGB = 1. */
static const double REC2020_TO_XYZ[3][3] = {
    {0.6369580483012914, 0.14461690358620832, 0.1688809751641721},
    {0.2627002120112671, 0.6779980715188708, 0.05930171646986196},
    {0.0, 0.028072693049087428, 1.060985057710791}
};

static const double LAB_WHITE[3] = {0.9504559270516718, 1.0, 1.0890577507598784};

static uint32_t curve_index(uint32_t j)
{
    uint32_t num = j * (LLK_NEUTRAL_POINTS - 1u);
    uint32_t den = LLK_CURVE_POINTS - 1u;

    return (num + den / 2u) / den;
}

static double ev_at(uint32_t index)
{
    return LLK_EV_MIN + (LLK_EV_MAX - LLK_EV_MIN) * (double)index /
                             (double)(LLK_NEUTRAL_POINTS - 1u);
}

/* Piecewise linear in EV; outside the knots the curve is held. */
static double curve_value(const float *curve, const float *ev, int n, double s)
{
    int i;

    if (s <= (double)ev[0]) {
        return (double)curve[0];
    }
    if (s >= (double)ev[n - 1]) {
        return (double)curve[n - 1];
    }
    i = 0;
    while (i < n - 2 && s > (double)ev[i + 1]) {
        i++;
    }
    {
        double span = (double)ev[i + 1] - (double)ev[i];
        double f = (s - (double)ev[i]) / span;

        return (double)curve[i] + ((double)curve[i + 1] - (double)curve[i]) * f;
    }
}

static double curve_slope(const float *curve, const float *ev, int n, double s)
{
    int i;

    if (s <= (double)ev[0]) {
        i = 0;
    } else if (s >= (double)ev[n - 1]) {
        i = n - 2;
    } else {
        i = 0;
        while (i < n - 2 && s > (double)ev[i + 1]) {
            i++;
        }
    }
    {
        double span = (double)ev[i + 1] - (double)ev[i];

        return ((double)curve[i + 1] - (double)curve[i]) / span;
    }
}

/*
 * Linear input whose curve value is y. Outputs at or below the first knot are
 * the black point, which no matrix value can distinguish, so it maps to 0.
 * Returns 0 when y is above the last knot.
 */
static int tone_inverse(const float *curve, const float *ev, int n, double y, double *x)
{
    int i;

    if (!(y > (double)curve[0])) {
        *x = 0.0;
        return 1;
    }
    if (y >= (double)curve[n - 1]) {
        return 0;
    }
    for (i = 0; i < n - 1; i++) {
        if (y <= (double)curve[i + 1]) {
            double span = (double)curve[i + 1] - (double)curve[i];
            double s = (double)ev[i] +
                       ((double)ev[i + 1] - (double)ev[i]) * (y - (double)curve[i]) / span;

            *x = LLK_MID_GRAY * pow(2.0, s);
            return 1;
        }
    }
    return 0;
}

static void rgb_to_xyz(const double rgb[3], double xyz[3])
{
    int row;

    for (row = 0; row < 3; row++) {
        xyz[row] = REC2020_TO_XYZ[row][0] * rgb[0] + REC2020_TO_XYZ[row][1] * rgb[1] +
                   REC2020_TO_XYZ[row][2] * rgb[2];
    }
}

static double lab_f(double t)
{
    const double eps = 216.0 / 24389.0;
    const double kappa = 24389.0 / 27.0;

    if (t > eps) {
        return cbrt(t);
    }
    return (kappa * t + 16.0) / 116.0;
}

static double lab_df(double t)
{
    const double eps = 216.0 / 24389.0;
    const double kappa = 24389.0 / 27.0;

    if (t > eps) {
        double c = cbrt(t);

        return 1.0 / (3.0 * c * c);
    }
    return kappa / 116.0;
}

static void lab_from_rgb(const double rgb[3], double lab[3])
{
    double xyz[3];
    double fx;
    double fy;
    double fz;

    rgb_to_xyz(rgb, xyz);
    fx = lab_f(xyz[0] / LAB_WHITE[0]);
    fy = lab_f(xyz[1] / LAB_WHITE[1]);
    fz = lab_f(xyz[2] / LAB_WHITE[2]);
    lab[0] = 116.0 * fy - 16.0;
    lab[1] = 500.0 * (fx - fy);
    lab[2] = 200.0 * (fy - fz);
}

static void lab_jacobian_rgb(const double rgb[3], double jac[3][3])
{
    double xyz[3];
    double dfx;
    double dfy;
    double dfz;
    double dl[3];
    double da[3];
    double db[3];
    int col;

    rgb_to_xyz(rgb, xyz);
    dfx = lab_df(xyz[0] / LAB_WHITE[0]) / LAB_WHITE[0];
    dfy = lab_df(xyz[1] / LAB_WHITE[1]) / LAB_WHITE[1];
    dfz = lab_df(xyz[2] / LAB_WHITE[2]) / LAB_WHITE[2];
    dl[0] = 0.0;
    dl[1] = 116.0 * dfy;
    dl[2] = 0.0;
    da[0] = 500.0 * dfx;
    da[1] = -500.0 * dfy;
    da[2] = 0.0;
    db[0] = 0.0;
    db[1] = 200.0 * dfy;
    db[2] = -200.0 * dfz;
    for (col = 0; col < 3; col++) {
        jac[0][col] = dl[0] * REC2020_TO_XYZ[0][col] + dl[1] * REC2020_TO_XYZ[1][col] +
                      dl[2] * REC2020_TO_XYZ[2][col];
        jac[1][col] = da[0] * REC2020_TO_XYZ[0][col] + da[1] * REC2020_TO_XYZ[1][col] +
                      da[2] * REC2020_TO_XYZ[2][col];
        jac[2][col] = db[0] * REC2020_TO_XYZ[0][col] + db[1] * REC2020_TO_XYZ[1][col] +
                      db[2] * REC2020_TO_XYZ[2][col];
    }
}

/* out = curve(m * in); dydv is d curve / d linear input per channel. */
static void predict(const llk_translation *t, double m[3][3], const double in[3],
                    double out[3], double dydv[3])
{
    int ch;

    for (ch = 0; ch < 3; ch++) {
        double v = m[ch][0] * in[0] + m[ch][1] * in[1] + m[ch][2] * in[2];
        double s;
        double slope;

        if (v > 0.0) {
            s = log2(v / LLK_MID_GRAY);
            if (s < (double)t->curve_ev[0]) {
                s = (double)t->curve_ev[0];
            }
            if (s > (double)t->curve_ev[t->curve_points - 1]) {
                s = (double)t->curve_ev[t->curve_points - 1];
            }
            slope = curve_slope(t->curve[ch], t->curve_ev, (int)t->curve_points, s);
            dydv[ch] = slope / (v * 0.6931471805599453);
        } else {
            s = (double)t->curve_ev[0];
            dydv[ch] = 0.0;
        }
        out[ch] = curve_value(t->curve[ch], t->curve_ev, (int)t->curve_points, s);
    }
}

static llk_status fit_stream(const llk_card *card, record_fn fn, void *ctx)
{
    uint8_t buffer[LLK_FIT_RECORD_LEN * 8u];
    uint32_t index = 0;

    while (index < card->fit_probes) {
        uint32_t batch = card->fit_probes - index;
        uint32_t k;

        if (batch > 8u) {
            batch = 8u;
        }
        if (card->reader.read(card->reader.ctx, card->fit_offset + 4u + index * LLK_FIT_RECORD_LEN,
                              buffer, batch * LLK_FIT_RECORD_LEN) != 0) {
            return LLK_ERR_IO;
        }
        for (k = 0; k < batch; k++) {
            const uint8_t *p = buffer + k * LLK_FIT_RECORD_LEN;
            fit_record record;
            int ch;

            for (ch = 0; ch < 3; ch++) {
                record.in[ch] = rdf32(p + (uint32_t)ch * 4u);
                record.out[ch] = rdf32(p + 12u + (uint32_t)ch * 4u);
            }
            record.sigma = rdf32(p + 24u);
            record.weight = rdf32(p + 28u);
            record.flags = rd32(p + 32u);
            record.probe = rd32(p + 36u);
            if (record.sigma != record.sigma || !isfinite(record.weight)) {
                return LLK_ERR_NOT_FINITE;
            }
            for (ch = 0; ch < 3; ch++) {
                if (!isfinite(record.in[ch]) || !isfinite(record.out[ch])) {
                    return LLK_ERR_NOT_FINITE;
                }
            }
            if (record.sigma < 0.0f || record.weight < 0.0f) {
                return LLK_ERR_SECTION;
            }
            if (fn(ctx, &record) != 0) {
                return LLK_ERR_IO;
            }
        }
        index += batch;
    }
    return LLK_OK;
}

typedef struct {
    const llk_translation *t;
    double a[3][3];
    double b[3][3];
    uint32_t used;
} seed_acc;

static int seed_record(void *ctx, const fit_record *record)
{
    seed_acc *acc = ctx;
    double in[3];
    double target[3];
    double u;
    int ch;
    int a;
    int b;

    acc->used++;
    if ((record->flags & 1u) == 0u || !(record->weight > 0.0f) || !isfinite(record->sigma)) {
        acc->used--;
        return 0;
    }
    for (ch = 0; ch < 3; ch++) {
        in[ch] = (double)record->in[ch];
        if (!tone_inverse(acc->t->curve[ch], acc->t->curve_ev, (int)acc->t->curve_points,
                          (double)record->out[ch], &target[ch])) {
            acc->used--;
            return 0;
        }
    }
    u = (double)record->weight / (1.0 + (double)record->sigma * (double)record->sigma);
    for (a = 0; a < 3; a++) {
        for (b = 0; b < 3; b++) {
            acc->a[a][b] += u * in[a] * in[b];
        }
        for (ch = 0; ch < 3; ch++) {
            acc->b[a][ch] += u * in[a] * target[ch];
        }
    }
    return 0;
}

/* Solves a * x = b for x, given b with one column per output channel. */
static int solve3(double a_in[3][3], double b_in[3][3], double x[3][3])
{
    double a[3][3];
    double b[3][3];
    int k;
    int i;
    int j;
    int ch;

    memcpy(a, a_in, sizeof a);
    memcpy(b, b_in, sizeof b);
    for (k = 0; k < 3; k++) {
        int pivot = k;
        double best = fabs(a[k][k]);

        for (i = k + 1; i < 3; i++) {
            if (fabs(a[i][k]) > best) {
                best = fabs(a[i][k]);
                pivot = i;
            }
        }
        if (best < 1e-12) {
            return 0;
        }
        if (pivot != k) {
            for (j = 0; j < 3; j++) {
                double tmp = a[k][j];

                a[k][j] = a[pivot][j];
                a[pivot][j] = tmp;
                tmp = b[k][j];
                b[k][j] = b[pivot][j];
                b[pivot][j] = tmp;
            }
        }
        for (i = k + 1; i < 3; i++) {
            double f = a[i][k] / a[k][k];

            for (j = k; j < 3; j++) {
                a[i][j] -= f * a[k][j];
            }
            for (j = 0; j < 3; j++) {
                b[i][j] -= f * b[k][j];
            }
        }
    }
    for (k = 2; k >= 0; k--) {
        for (ch = 0; ch < 3; ch++) {
            double sum = b[k][ch];

            for (j = k + 1; j < 3; j++) {
                sum -= a[k][j] * x[j][ch];
            }
            x[k][ch] = sum / a[k][k];
        }
    }
    return 1;
}

static int solve9(double h[9][9], const double g[9], double delta[9])
{
    double a[9][10];
    int k;
    int i;
    int j;

    for (i = 0; i < 9; i++) {
        for (j = 0; j < 9; j++) {
            a[i][j] = h[i][j];
        }
        a[i][9] = -g[i];
    }
    for (k = 0; k < 9; k++) {
        int pivot = k;
        double best = fabs(a[k][k]);

        for (i = k + 1; i < 9; i++) {
            if (fabs(a[i][k]) > best) {
                best = fabs(a[i][k]);
                pivot = i;
            }
        }
        if (best < 1e-18) {
            return 0;
        }
        if (pivot != k) {
            for (j = k; j < 10; j++) {
                double tmp = a[k][j];

                a[k][j] = a[pivot][j];
                a[pivot][j] = tmp;
            }
        }
        for (i = k + 1; i < 9; i++) {
            double f = a[i][k] / a[k][k];

            for (j = k; j < 10; j++) {
                a[i][j] -= f * a[k][j];
            }
        }
    }
    for (k = 8; k >= 0; k--) {
        double sum = a[k][9];

        for (j = k + 1; j < 9; j++) {
            sum -= a[k][j] * delta[j];
        }
        delta[k] = sum / a[k][k];
    }
    return 1;
}

typedef struct {
    const llk_translation *t;
    double m[3][3];
    int with_jacobian;
    double h[9][9];
    double g[9];
    double loss;
    double max_de;
    double weight_sum;
    uint32_t probes;
    uint32_t supported;
    uint32_t used;
} fit_pass;

static int pass_record(void *ctx, const fit_record *record)
{
    fit_pass *pass = ctx;
    double in[3];
    double out[3];
    double target_lab[3];
    double predicted_lab[3];
    double prediction[3];
    double dydv[3];
    double u;
    int ch;

    pass->probes++;
    if ((record->flags & 1u) == 0u) {
        return 0;
    }
    pass->supported++;
    if (!(record->weight > 0.0f) || !isfinite(record->sigma)) {
        return 0;
    }
    for (ch = 0; ch < 3; ch++) {
        if ((double)record->out[ch] < (double)pass->t->curve[ch][0] ||
            (double)record->out[ch] >=
                (double)pass->t->curve[ch][pass->t->curve_points - 1]) {
            return 0;
        }
        in[ch] = (double)record->in[ch];
        out[ch] = (double)record->out[ch];
    }
    u = (double)record->weight / (1.0 + (double)record->sigma * (double)record->sigma);
    predict(pass->t, pass->m, in, prediction, dydv);
    lab_from_rgb(out, target_lab);
    lab_from_rgb(prediction, predicted_lab);
    {
        double delta[3];
        int k;

        for (k = 0; k < 3; k++) {
            delta[k] = predicted_lab[k] - target_lab[k];
        }
        pass->loss += u * (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]);
        pass->weight_sum += u;
        {
            double de =
                sqrt(delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]);

            if (de > pass->max_de) {
                pass->max_de = de;
            }
        }
        if (pass->with_jacobian) {
            double jac[3][3];
            double j[3][9];

            lab_jacobian_rgb(prediction, jac);
            for (k = 0; k < 3; k++) {
                int a;
                int b;

                for (a = 0; a < 3; a++) {
                    double factor = jac[k][a] * dydv[a];

                    for (b = 0; b < 3; b++) {
                        j[k][a * 3 + b] = factor * in[b];
                    }
                }
            }
            for (k = 0; k < 9; k++) {
                int b;

                for (b = 0; b < 9; b++) {
                    pass->h[k][b] += u * j[0][k] * j[0][b] + u * j[1][k] * j[1][b] +
                                     u * j[2][k] * j[2][b];
                }
                pass->g[k] += u * (j[0][k] * delta[0] + j[1][k] * delta[1] +
                                   j[2][k] * delta[2]);
            }
        }
        pass->used++;
    }
    return 0;
}

static llk_status measure(const llk_card *card, const llk_translation *t, double m[3][3],
                          int with_jacobian, fit_pass *pass)
{
    llk_status status;

    memset(pass, 0, sizeof *pass);
    pass->t = t;
    memcpy(pass->m, m, sizeof pass->m);
    pass->with_jacobian = with_jacobian;
    status = fit_stream(card, pass_record, pass);
    if (status != LLK_OK) {
        return status;
    }
    return LLK_OK;
}

static llk_status compute_seed(const llk_card *card, const llk_translation *t, double m[3][3])
{
    seed_acc acc;
    llk_status status;
    double x[3][3];
    int ch;
    int comp;

    memset(&acc, 0, sizeof acc);
    acc.t = t;
    status = fit_stream(card, seed_record, &acc);
    if (status != LLK_OK) {
        return status;
    }
    if (acc.used < 4u) {
        return LLK_ERR_NO_PROBES;
    }
    if (!solve3(acc.a, acc.b, x)) {
        return LLK_ERR_DEGENERATE;
    }
    for (ch = 0; ch < 3; ch++) {
        for (comp = 0; comp < 3; comp++) {
            m[ch][comp] = x[comp][ch];
        }
    }
    return LLK_OK;
}

static llk_status refine_lab(const llk_card *card, const llk_translation *t, double m[3][3],
                             llk_translation *out)
{
    fit_pass pass;
    double best_loss;
    llk_status status;
    int iteration;

    status = measure(card, t, m, 0, &pass);
    if (status != LLK_OK) {
        return status;
    }
    if (pass.used == 0u) {
        return LLK_ERR_NO_PROBES;
    }
    best_loss = pass.loss;

    for (iteration = 0; iteration < LLK_GN_ITERATIONS; iteration++) {
        double h[9][9];
        double g[9];
        double scale = 0.0;
        double lambda;
        int attempt;
        int accepted = 0;
        int a;
        int b;

        status = measure(card, t, m, 1, &pass);
        if (status != LLK_OK) {
            return status;
        }
        memcpy(h, pass.h, sizeof h);
        memcpy(g, pass.g, sizeof g);
        for (a = 0; a < 9; a++) {
            scale += h[a][a];
        }
        scale = scale / 9.0;
        if (!(scale > 0.0)) {
            break;
        }
        lambda = 1e-9 * scale;

        for (attempt = 0; attempt < LLK_GN_ATTEMPTS; attempt++) {
            double delta[9];
            double trial[3][3];
            double loss;

            {
                double damped[9][9];

                memcpy(damped, h, sizeof damped);
                for (a = 0; a < 9; a++) {
                    damped[a][a] += lambda * (1.0 + h[a][a]);
                }
                if (!solve9(damped, g, delta)) {
                    lambda *= 100.0;
                    continue;
                }
            }
            for (a = 0; a < 3; a++) {
                for (b = 0; b < 3; b++) {
                    trial[a][b] = m[a][b] + delta[a * 3 + b];
                }
            }
            status = measure(card, t, trial, 0, &pass);
            if (status != LLK_OK) {
                return status;
            }
            loss = pass.loss;
            if (pass.used > 0u && loss < best_loss) {
                for (a = 0; a < 3; a++) {
                    for (b = 0; b < 3; b++) {
                        m[a][b] = trial[a][b];
                    }
                }
                best_loss = loss;
                accepted = 1;
                break;
            }
            lambda *= 10.0;
        }
        if (!accepted) {
            break;
        }
    }

    status = measure(card, t, m, 0, &pass);
    if (status != LLK_OK) {
        return status;
    }
    if (pass.used == 0u) {
        return LLK_ERR_NO_PROBES;
    }
    out->probes = pass.probes;
    out->supported = pass.supported;
    out->fitted = pass.used;
    out->lab_rms = pass.weight_sum > 0.0 ? sqrt(pass.loss / pass.weight_sum) : 0.0;
    out->lab_max = pass.max_de;
    return LLK_OK;
}

llk_status llk_translate(const llk_card *card, llk_translation *out)
{
    llk_translation t;
    double m[3][3];
    llk_status status;
    int ch;
    int j;

    if (card == NULL || out == NULL) {
        return LLK_ERR_IO;
    }
    memset(&t, 0, sizeof t);
    t.curve_points = LLK_CURVE_POINTS;
    for (j = 0; j < LLK_CURVE_POINTS; j++) {
        uint32_t index = curve_index((uint32_t)j);
        double ev = ev_at(index);

        t.curve_ev[j] = (float)ev;
        t.curve_input[j] = (float)(LLK_MID_GRAY * pow(2.0, ev));
        for (ch = 0; ch < 3; ch++) {
            t.curve[ch][j] = card->neutral[index][ch];
        }
    }

    status = compute_seed(card, &t, m);
    if (status != LLK_OK) {
        return status;
    }
    status = refine_lab(card, &t, m, &t);
    if (status != LLK_OK) {
        return status;
    }
    for (ch = 0; ch < 3; ch++) {
        for (j = 0; j < 3; j++) {
            t.matrix[ch][j] = (float)m[ch][j];
        }
    }
    *out = t;
    return LLK_OK;
}

double llk_curve_eval(const llk_translation *t, int channel, double x)
{
    int points;
    double s;

    if (t == NULL || channel < 0 || channel > 2) {
        return 0.0;
    }
    points = (int)t->curve_points;
    if (points < 2 || points > LLK_CURVE_POINTS) {
        points = LLK_CURVE_POINTS;
    }
    s = (x > 0.0) ? log2(x / LLK_MID_GRAY) : (double)t->curve_ev[0];
    return curve_value(t->curve[channel], t->curve_ev, points, s);
}

void llk_apply(const llk_translation *t, const float in[3], float out[3])
{
    int ch;

    for (ch = 0; ch < 3; ch++) {
        double v = (double)t->matrix[ch][0] * (double)in[0] +
                   (double)t->matrix[ch][1] * (double)in[1] +
                   (double)t->matrix[ch][2] * (double)in[2];

        out[ch] = (float)llk_curve_eval(t, ch, v);
    }
}
