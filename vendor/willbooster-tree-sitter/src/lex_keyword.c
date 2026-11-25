#include "./lex_table.h"
#include "./unicode.h"

typedef struct {
  uint32_t offset;
  uint16_t state;
  bool is_keyword;
} LexKeywordProgress;

static inline bool ts_lex_can_lookup_keyword(const uint16_t *table, TSInputEncoding encoding);
static inline TSLexKeywordResult ts_lex_lookup_keyword_ascii(
  const TSLexTableView *v, const uint8_t *bytes, uint32_t byte_len, bool utf16,
  LexKeywordProgress *prog
);
static inline uint16_t ts_lex_read_keyword_unit(const uint8_t *bytes, uint32_t idx, bool utf16);
static TS_NOINLINE TSLexKeywordResult ts_lex_lookup_keyword_from(
  const TSLexTableView *v, const uint8_t *bytes, uint32_t byte_len, TSInputEncoding encoding,
  LexKeywordProgress prog
);
static inline TSLexKeywordResult ts_lex_finish_keyword_state(
  const TSLexTableView *v, uint16_t state, bool is_keyword
);
static inline bool ts_lex_keyword_step(const TSLexTableView *v, uint16_t act, uint16_t *state);
static inline uint32_t ts_lex_decode_keyword_char(
  const uint8_t *bytes, uint32_t remaining, TSInputEncoding encoding, int32_t *cp
);

TSLexKeywordResult ts_lexer_lookup_keyword(
  Lexer *self,
  const uint16_t *table,
  const uint8_t *bytes,
  uint32_t byte_len,
  TSInputEncoding encoding
) {
  if (TS_UNLIKELY(!ts_lex_can_lookup_keyword(table, encoding))) {
    return (TSLexKeywordResult){0, false, false};
  }
  const TSLexTableView *v = TS_LIKELY(self->kw_lex_cache.table == table)
    ? &self->kw_lex_cache
    : ts_lex_select_view_cold(self, table);
  LexKeywordProgress prog = {0, 0, false};
  if (TS_LIKELY(!v->wide_base)) {
    if (TS_LIKELY(encoding == TSInputEncodingUTF8)) {
      TSLexKeywordResult res = ts_lex_lookup_keyword_ascii(v, bytes, byte_len, false, &prog);
      if (TS_LIKELY(prog.offset == byte_len || res.handled || prog.offset == UINT32_MAX)) {
        return res;
      }
    } else if (encoding == TSInputEncodingUTF16LE) {
      TSLexKeywordResult res = ts_lex_lookup_keyword_ascii(v, bytes, byte_len, true, &prog);
      if (prog.offset == byte_len || res.handled || prog.offset == UINT32_MAX) return res;
    }
  }
  return ts_lex_lookup_keyword_from(
    v, bytes + prog.offset, byte_len - prog.offset, encoding, prog
  );
}

static inline bool ts_lex_can_lookup_keyword(const uint16_t *table, TSInputEncoding encoding) {
  return table &&
    (table[6] & TS_LEX_DIRECT_KEYWORD_FLAG) &&
    (encoding == TSInputEncodingUTF8 ||
     encoding == TSInputEncodingUTF16LE ||
     encoding == TSInputEncodingUTF16BE);
}

static inline TSLexKeywordResult ts_lex_lookup_keyword_ascii(
  const TSLexTableView *v,
  const uint8_t *bytes,
  uint32_t byte_len,
  bool utf16,
  LexKeywordProgress *prog
) {
  uint32_t stride = utf16 ? 2u : 1u;
  uint32_t idx = 0;
  uint16_t state = 0;
  bool kw = false;
  while (idx + stride <= byte_len) {
    uint16_t u = ts_lex_read_keyword_unit(bytes, idx, utf16);
    if (TS_UNLIKELY(u >= 0x80u)) {
      *prog = (LexKeywordProgress){idx, state, kw};
      return (TSLexKeywordResult){0, false, false};
    }
    const uint16_t *sm = &v->state_meta[(uint32_t)state * 4u];
    if (sm[3] != 0) kw = true;
    uint16_t tmpl = sm[1];
    if (tmpl == 0) return (TSLexKeywordResult){0, true, kw};
    const uint16_t *ce = &v->comb[((uint32_t)sm[0] + v->ascii_class[u]) * 2u];
    uint16_t act = ce[0] == tmpl ? ce[1] : sm[2];
    if (act == 0) return (TSLexKeywordResult){0, true, kw};
    if (TS_UNLIKELY(!ts_lex_keyword_step(v, act, &state))) {
      prog->offset = UINT32_MAX;
      return (TSLexKeywordResult){0, false, false};
    }
    idx += stride;
  }
  *prog = (LexKeywordProgress){idx, state, kw};
  return idx == byte_len
    ? ts_lex_finish_keyword_state(v, state, kw)
    : (TSLexKeywordResult){0, false, false};
}

static inline uint16_t ts_lex_read_keyword_unit(const uint8_t *bytes, uint32_t idx, bool utf16) {
  return utf16
    ? (uint16_t)((uint16_t)bytes[idx] | ((uint16_t)bytes[idx + 1u] << 8))
    : bytes[idx];
}

static TS_NOINLINE TSLexKeywordResult ts_lex_lookup_keyword_from(
  const TSLexTableView *v,
  const uint8_t *bytes,
  uint32_t byte_len,
  TSInputEncoding encoding,
  LexKeywordProgress prog
) {
  uint32_t offset = 0;
  while (offset < byte_len) {
    TSLexStateMeta sm = ts_lex_state_meta(v, prog.state);
    if (sm.acc != 0) prog.is_keyword = true;
    int32_t cp = 0;
    uint32_t step = ts_lex_decode_keyword_char(bytes + offset, byte_len - offset, encoding, &cp);
    if (step == 0) break;
    offset += step;
    uint16_t act = ts_lex_lookup_action(v, sm, ts_lex_char_class(v, cp, false));
    if (act == 0) return (TSLexKeywordResult){0, true, prog.is_keyword};
    if (!ts_lex_keyword_step(v, act, &prog.state)) return (TSLexKeywordResult){0, false, false};
  }
  return ts_lex_finish_keyword_state(v, prog.state, prog.is_keyword);
}

static inline TSLexKeywordResult ts_lex_finish_keyword_state(
  const TSLexTableView *v,
  uint16_t state,
  bool is_keyword
) {
  uint16_t acc = v->state_meta[(uint32_t)state * 4u + 3u];
  if (acc != 0) {
    return (TSLexKeywordResult){(TSSymbol)((acc & TS_LEX_SIMPLE_ACCEPT_MASK) - 1u), true, true};
  }
  return (TSLexKeywordResult){0, true, is_keyword};
}

static inline bool ts_lex_keyword_step(const TSLexTableView *v, uint16_t act, uint16_t *state) {
  if (TS_LIKELY(act < TS_LEX_EXTENDED_FLAG)) {
    if (TS_UNLIKELY(act & TS_LEX_SIMPLE_SKIP_FLAG)) return false;
    *state = (uint16_t)((act & TS_LEX_SIMPLE_STATE_MASK) - 1u);
    return true;
  }
  const uint16_t *p = &v->ext_transitions[act & 0x7fffu];
  if (p[1] & TS_LEX_EXTENDED_FLAG) return false;
  *state = p[0];
  return true;
}

static inline uint32_t ts_lex_decode_keyword_char(
  const uint8_t *bytes,
  uint32_t remaining,
  TSInputEncoding encoding,
  int32_t *cp
) {
  if (encoding == TSInputEncodingUTF8 && bytes[0] < 0x80u) {
    *cp = bytes[0];
    return 1;
  }
  if (encoding == TSInputEncodingUTF16LE && remaining >= 2 && bytes[0] < 0x80u && bytes[1] == 0) {
    *cp = bytes[0];
    return 2;
  }
  uint32_t step = encoding == TSInputEncodingUTF8 ? ts_decode_utf8(bytes, remaining, cp)
    : encoding == TSInputEncodingUTF16LE ? ts_decode_utf16_le(bytes, remaining, cp)
    : ts_decode_utf16_be(bytes, remaining, cp);
  return *cp == TS_DECODE_ERROR ? 1 : step;
}
