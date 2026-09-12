// SPDX-License-Identifier: GPL-2.0
/* Host adapter for the kernel's exact RS codec (0x11d, fcr=0, prim=1).
 * Compile with -I <kernel>/lib/reed_solomon. The codec bodies retain their
 * upstream license and authorship; they are included directly, not vendored.
 */
#include <assert.h>
#include <errno.h>
#include <stdint.h>
#include <string.h>

#define BUG_ON(x) assert(!(x))
#define min(a, b) ((a) < (b) ? (a) : (b))
enum { RS_DECODE_LAMBDA, RS_DECODE_SYN, RS_DECODE_B, RS_DECODE_T,
       RS_DECODE_OMEGA, RS_DECODE_ROOT, RS_DECODE_REG, RS_DECODE_LOC };
struct rs_codec {
    int nn, nroots, fcr, prim, iprim;
    uint16_t alpha_to[256], index_of[256], genpoly[129];
};
struct rs_control { struct rs_codec *codec; uint16_t buffers[8 * 129]; };
static int rs_modnn(struct rs_codec *rs, int x) { return x % rs->nn; }

static int encode_rs8(struct rs_control *rsc, uint8_t *data, int len,
                      uint16_t *par, uint16_t invmsk)
{
#include "encode_rs.c"
}
static int decode_rs8(struct rs_control *rsc, uint8_t *data, uint16_t *par,
                      int len, uint16_t *s, int no_eras, int *eras_pos,
                      uint16_t invmsk, uint16_t *corr)
{
#include "decode_rs.c"
}

static void setup(struct rs_codec *rs, int roots)
{
    memset(rs, 0, sizeof(*rs));
    rs->nn = 255; rs->nroots = roots; rs->prim = rs->iprim = 1;
    rs->index_of[0] = 255;
    for (int i = 0, value = 1; i < 255; i++) {
        rs->alpha_to[i] = value; rs->index_of[value] = i;
        value <<= 1;
        if (value & 256) value ^= 0x11d;
    }
    rs->genpoly[0] = 1;
    for (int i = 0; i < roots; i++) {
        rs->genpoly[i + 1] = 1;
        for (int j = i; j > 0; j--)
            rs->genpoly[j] = rs->genpoly[j - 1] ^ (rs->genpoly[j] ?
                rs->alpha_to[(rs->index_of[rs->genpoly[j]] + i) % 255] : 0);
        rs->genpoly[0] = rs->alpha_to[(rs->index_of[rs->genpoly[0]] + i) % 255];
    }
    for (int i = 0; i <= roots; i++) rs->genpoly[i] = rs->index_of[rs->genpoly[i]];
}

int pb_rs(uint8_t *data, int len, uint8_t *parity, int roots, int encode)
{
    if (roots < 1 || roots > 127 || len < 1 || len + roots > 255) return -EINVAL;
    struct rs_codec codec;
    struct rs_control control = { .codec = &codec };
    uint16_t parity16[128] = {0};
    setup(&codec, roots);
    for (int i = 0; i < roots; i++) if (!encode) parity16[i] = parity[i];
    int result = encode ? encode_rs8(&control, data, len, parity16, 0) :
        decode_rs8(&control, data, parity16, len, NULL, 0, NULL, 0, NULL);
    for (int i = 0; i < roots; i++) parity[i] = parity16[i];
    return result;
}
