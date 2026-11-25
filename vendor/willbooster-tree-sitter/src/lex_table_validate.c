#include "./lex_table.h"

static bool ts_lex_validate_header(const uint16_t *table, uint32_t max_words);
static bool ts_lex_validate_classes(const TSLexTableView *v);
static bool ts_lex_validate_states_and_comb(const TSLexTableView *v, uint32_t symbol_count);
static bool ts_lex_validate_accept(const TSLexTableView *v, uint16_t acc, uint32_t symbol_count);
static bool ts_lex_validate_ext_accept_body(
  const TSLexTableView *v, uint16_t flags, uint32_t off, uint32_t symbol_count
);
static bool ts_lex_validate_action(const TSLexTableView *v, uint16_t act);
static bool ts_lex_validate_ext_action(const TSLexTableView *v, uint32_t off);

uint32_t ts_lexer_validate_table(
  const uint16_t *table,
  uint32_t max_words,
  uint32_t symbol_count
) {
  if (!ts_lex_validate_header(table, max_words)) return 0;
  TSLexTableView v = ts_lex_unpack_view(table);
  if (!ts_lex_validate_classes(&v)) return 0;
  if (!ts_lex_validate_states_and_comb(&v, symbol_count)) return 0;
  return v.total_words;
}

static bool ts_lex_validate_header(const uint16_t *table, uint32_t max_words) {
  if (!table || max_words < TS_LEX_MIN_TABLE_WORDS) return false;
  uint32_t total_words = ts_lex_read_u32(table);
  if (total_words < TS_LEX_MIN_TABLE_WORDS || total_words > max_words) return false;
  uint16_t state_count = table[2];
  uint16_t template_count = table[3];
  uint16_t class_count = table[4];
  uint16_t unicode_ranges = table[5];
  uint16_t tag_count = table[6] & TS_LEX_TAG_COUNT_MASK;
  bool wide_base = (table[6] & TS_LEX_WIDE_BASE_FLAG) != 0;
  bool direct_kw = (table[6] & TS_LEX_DIRECT_KEYWORD_FLAG) != 0;
  uint32_t comb_len = ts_lex_read_u32(table + 7);
  uint32_t ext_trans_len = ts_lex_read_u32(table + 9);
  uint32_t ext_acc_len = ts_lex_read_u32(table + 11);
  if (!state_count || !template_count || class_count < 2 || !unicode_ranges || comb_len < class_count) {
    return false;
  }
  if (direct_kw && (tag_count > 1 || ext_acc_len != 0)) return false;
  uint64_t state_stride = wide_base ? 5ull : 4ull;
  uint64_t expected = 141ull + (uint64_t)unicode_ranges * 3ull +
    (uint64_t)state_count * state_stride + (uint64_t)comb_len * 2ull +
    ext_trans_len + ext_acc_len;
  return expected == total_words;
}

static bool ts_lex_validate_classes(const TSLexTableView *v) {
  for (uint32_t i = 0; i < 128; i++) {
    if (v->ascii_class[i] >= v->class_count) return false;
  }
  uint32_t prev_end = 0x7fu;
  for (uint32_t i = 0; i < v->unicode_range_count; i++) {
    uint32_t end = ts_lex_read_u32(&v->unicode_ends[i * 2u]);
    if (end <= prev_end || end > 0x10ffffu || v->unicode_classes[i] >= v->class_count) {
      return false;
    }
    prev_end = end;
  }
  return prev_end == 0x10ffffu;
}

static bool ts_lex_validate_states_and_comb(const TSLexTableView *v, uint32_t symbol_count) {
  uint32_t max_base = v->comb_len - v->class_count;
  for (uint32_t s = 0; s < v->state_count; s++) {
    TSLexStateMeta sm = ts_lex_state_meta(v, (uint16_t)s);
    if (sm.tmpl >= v->template_count || sm.base > max_base) return false;
    if (sm.tmpl == 0 && (sm.base != 0 || sm.def != 0)) return false;
    if (!ts_lex_validate_action(v, sm.def) || !ts_lex_validate_accept(v, sm.acc, symbol_count)) {
      return false;
    }
  }
  for (uint32_t i = 0; i < v->comb_len; i++) {
    const uint16_t *ce = &v->comb[i * 2u];
    if (ce[0] < v->template_count && !ts_lex_validate_action(v, ce[1])) return false;
  }
  return true;
}

static bool ts_lex_validate_accept(
  const TSLexTableView *v,
  uint16_t acc,
  uint32_t symbol_count
) {
  if (acc == 0) return true;
  if (acc < TS_LEX_EXTENDED_FLAG) {
    if ((acc & TS_LEX_SIMPLE_ACCEPT_TAG0_FLAG) && v->tag_count == 0) return false;
    uint16_t raw = acc & TS_LEX_SIMPLE_ACCEPT_MASK;
    return raw != 0 && (uint32_t)(raw - 1u) < symbol_count;
  }
  if (v->direct_keyword_lookup) return false;
  uint32_t off = acc & 0x7fffu;
  if (off >= v->ext_accepts_len) return false;
  uint16_t flags = v->ext_accepts[off++];
  bool invalid_flags = (flags & ~0x1fu) ||
    ((flags & 2u) && (flags & 0x1cu)) ||
    (!(flags & 4u) && (flags & 0x18u));
  if (invalid_flags) return false;
  return ts_lex_validate_ext_accept_body(v, flags, off, symbol_count);
}

static bool ts_lex_validate_ext_accept_body(
  const TSLexTableView *v,
  uint16_t flags,
  uint32_t off,
  uint32_t symbol_count
) {
  if (flags & 1u) {
    if (off + 4u > v->ext_accepts_len || v->ext_accepts[off] >= symbol_count) return false;
    uint16_t tag = v->ext_accepts[off + 3u];
    if (tag != UINT16_MAX && tag >= v->tag_count) return false;
    off += 4u;
  }
  if (!(flags & 4u)) return true;
  uint32_t need = 1u + ((flags & 8u) ? 2u : 0u) + ((flags & 16u) ? 1u : 0u);
  if (off + need > v->ext_accepts_len || v->ext_accepts[off] >= symbol_count) return false;
  return !(flags & 16u) || v->ext_accepts[off + need - 1u] < v->tag_count;
}

static bool ts_lex_validate_action(const TSLexTableView *v, uint16_t act) {
  if (act == 0) return true;
  if (act < TS_LEX_EXTENDED_FLAG) {
    if ((act & TS_LEX_SIMPLE_TAG0_FLAG) && v->tag_count == 0) return false;
    uint16_t raw = act & TS_LEX_SIMPLE_STATE_MASK;
    return raw != 0 && (uint32_t)(raw - 1u) < v->state_count;
  }
  return ts_lex_validate_ext_action(v, act & 0x7fffu);
}

static bool ts_lex_validate_ext_action(const TSLexTableView *v, uint32_t off) {
  if (off + 2u > v->ext_transitions_len) return false;
  uint16_t next_state = v->ext_transitions[off];
  uint16_t op_count = v->ext_transitions[off + 1u] & 0x7fffu;
  if (next_state >= v->state_count) return false;
  if (v->direct_keyword_lookup && op_count > 1) return false;
  if (off + 2u + (uint32_t)op_count * 2u > v->ext_transitions_len) return false;
  for (uint32_t k = 0; k < op_count; k++) {
    uint16_t target = v->ext_transitions[off + 2u + k * 2u];
    uint16_t source = v->ext_transitions[off + 3u + k * 2u];
    if (target >= v->tag_count || (source != UINT16_MAX && source >= v->tag_count)) return false;
    if (v->direct_keyword_lookup && (target != 0 || source != UINT16_MAX)) return false;
  }
  return true;
}
