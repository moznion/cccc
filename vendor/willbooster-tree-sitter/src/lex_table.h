#ifndef TREE_SITTER_LEX_TABLE_H_
#define TREE_SITTER_LEX_TABLE_H_

#ifdef __cplusplus
extern "C" {
#endif

#include "./alloc.h"
#include "./lexer.h"

#define TS_LEX_DIRECT_KEYWORD_FLAG 0x8000u
#define TS_LEX_WIDE_BASE_FLAG 0x4000u
#define TS_LEX_TAG_COUNT_MASK 0x3fffu
#define TS_LEX_EXTENDED_FLAG 0x8000u
#define TS_LEX_SIMPLE_SKIP_FLAG 0x4000u
#define TS_LEX_SIMPLE_TAG0_FLAG 0x2000u
#define TS_LEX_SIMPLE_STATE_MASK 0x1fffu
#define TS_LEX_SIMPLE_ACCEPT_TAG0_FLAG 0x4000u
#define TS_LEX_SIMPLE_ACCEPT_MASK 0x3fffu
#define TS_LEX_MIN_TABLE_WORDS 141u

typedef struct {
  TSSymbol candidate;
  bool handled;
  bool is_keyword;
} TSLexKeywordResult;

typedef struct {
  uint32_t base;
  uint16_t tmpl;
  uint16_t def;
  uint16_t acc;
} TSLexStateMeta;

static inline uint32_t ts_lex_read_u32(const uint16_t *words) {
  return (uint32_t)words[0] | ((uint32_t)words[1] << 16);
}

static inline TSLexTableView ts_lex_unpack_view(const uint16_t *t) {
  uint32_t state_count = t[2];
  uint32_t unicode_ranges = t[5];
  bool wide_base = (t[6] & TS_LEX_WIDE_BASE_FLAG) != 0;
  uint32_t comb_len = ts_lex_read_u32(t + 7);
  uint32_t ext_trans_len = ts_lex_read_u32(t + 9);
  const uint16_t *unicode_ends = t + 141;
  const uint16_t *state_meta = unicode_ends + unicode_ranges * 3u;
  const uint16_t *comb = state_meta + state_count * (wide_base ? 5u : 4u);
  const uint16_t *ext_transitions = comb + comb_len * 2u;
  return (TSLexTableView) {
    .table = t,
    .total_words = ts_lex_read_u32(t),
    .state_count = (uint16_t)state_count,
    .template_count = t[3],
    .class_count = t[4],
    .unicode_range_count = (uint16_t)unicode_ranges,
    .tag_count = t[6] & TS_LEX_TAG_COUNT_MASK,
    .wide_base = wide_base,
    .direct_keyword_lookup = (t[6] & TS_LEX_DIRECT_KEYWORD_FLAG) != 0,
    .comb_len = comb_len,
    .ext_transitions_len = ext_trans_len,
    .ext_accepts_len = ts_lex_read_u32(t + 11),
    .ascii_class = t + 13,
    .unicode_ends = unicode_ends,
    .unicode_classes = unicode_ends + unicode_ranges * 2u,
    .state_meta = state_meta,
    .comb = comb,
    .ext_transitions = ext_transitions,
    .ext_accepts = ext_transitions + ext_trans_len,
  };
}

static inline TSLexStateMeta ts_lex_state_meta(const TSLexTableView *v, uint16_t state) {
  if (!v->wide_base) {
    const uint16_t *sm = &v->state_meta[(uint32_t)state * 4u];
    return (TSLexStateMeta){sm[0], sm[1], sm[2], sm[3]};
  }
  const uint16_t *sm = &v->state_meta[(uint32_t)state * 5u];
  return (TSLexStateMeta){ts_lex_read_u32(sm), sm[2], sm[3], sm[4]};
}

static inline uint16_t ts_lex_unicode_range(
  const TSLexTableView *v,
  uint32_t cp,
  uint32_t *range_lo,
  uint32_t *range_span
) {
  uint32_t low = 0;
  uint32_t high = v->unicode_range_count - 1u;
  while (low < high) {
    uint32_t mid = low + (high - low) / 2u;
    if (cp > ts_lex_read_u32(&v->unicode_ends[mid * 2u])) {
      low = mid + 1u;
    } else {
      high = mid;
    }
  }
  uint32_t lo = low == 0 ? 0x80u : ts_lex_read_u32(&v->unicode_ends[(low - 1u) * 2u]) + 1u;
  uint32_t hi = ts_lex_read_u32(&v->unicode_ends[low * 2u]);
  *range_lo = lo;
  *range_span = hi - lo;
  return v->unicode_classes[low];
}

static inline uint16_t ts_lex_unicode_class(const TSLexTableView *v, uint32_t cp) {
  uint32_t lo = 0;
  uint32_t span = 0;
  return ts_lex_unicode_range(v, cp, &lo, &span);
}

static inline uint16_t ts_lex_char_class(const TSLexTableView *v, int32_t lookahead, bool eof) {
  if (eof) return 0;
  uint32_t cp = (uint32_t)lookahead;
  if (cp < 0x80u) return v->ascii_class[cp];
  if (cp < 0x110000u) return ts_lex_unicode_class(v, cp);
  return UINT16_MAX;
}

static inline uint16_t ts_lex_lookup_action(
  const TSLexTableView *v,
  TSLexStateMeta sm,
  uint16_t cls
) {
  if (sm.tmpl == 0 || cls == UINT16_MAX) return 0;
  const uint16_t *ce = &v->comb[(sm.base + cls) * 2u];
  return ce[0] == sm.tmpl ? ce[1] : sm.def;
}

static inline bool ts_lex_action_is_skip(const TSLexTableView *v, uint16_t act) {
  if (act == 0) return false;
  if (act < TS_LEX_EXTENDED_FLAG) return (act & TS_LEX_SIMPLE_SKIP_FLAG) != 0;
  return (v->ext_transitions[(act & 0x7fffu) + 1u] & TS_LEX_EXTENDED_FLAG) != 0;
}

static inline uint16_t ts_lex_action_next_state(const TSLexTableView *v, uint16_t act) {
  if (act < TS_LEX_EXTENDED_FLAG) return (uint16_t)((act & TS_LEX_SIMPLE_STATE_MASK) - 1u);
  return v->ext_transitions[act & 0x7fffu];
}

typedef struct {
  Length pos;
  ColumnData col;
  ColumnData disp;
} TSLexDeadEndTarget;

bool ts_lex_lookup_dead_end(
  const TSLexDeadEndCache *cache,
  const uint16_t *table,
  uint16_t state,
  uint32_t byte,
  TSLexDeadEndTarget *target
);

void ts_lex_record_dead_ends(
  Lexer *self,
  const TSLexTableView *v,
  uint16_t start_state,
  Length start_pos
);

TSLexTableView *ts_lex_select_view_cold(Lexer *self, const uint16_t *table);

static inline TSLexTableView *ts_lex_select_view(Lexer *self, const uint16_t *table) {
  if (TS_LIKELY(self->main_lex_cache.table == table)) return &self->main_lex_cache;
  return ts_lex_select_view_cold(self, table);
}

bool ts_lexer_lex_table(Lexer *self, const uint16_t *table, TSStateId start_state);

TSLexKeywordResult ts_lexer_lookup_keyword(
  Lexer *self,
  const uint16_t *table,
  const uint8_t *bytes,
  uint32_t byte_len,
  TSInputEncoding encoding
);

uint32_t ts_lexer_validate_table(
  const uint16_t *table,
  uint32_t max_words,
  uint32_t symbol_count
);

#ifdef __cplusplus
}
#endif

#endif  // TREE_SITTER_LEX_TABLE_H_
