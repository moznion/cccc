// code-gauge-allow-file fileNcss -- keeps DFA execution, Unicode exception bounds, and accept guards together
#include "./lex_table.h"
#include "./unicode.h"

#include <string.h>

#define TS_LEX_STACK_TAG_CAPACITY 16u

typedef struct {
  bool valid;
  bool has_decode_error;
  int32_t precedence;
  TSSymbol symbol;
  TSLexerCheckpoint start;
  TSLexerCheckpoint end;
} LexFallback;

typedef struct {
  LexFallback fb;
  TSLexerCheckpoint *tags;
  bool result;
} LexRunState;

typedef struct {
  Length initial_pos;
  TSLexerCheckpoint tag0;
  uint16_t initial_state;
  uint16_t state;
  bool result;
} LexSlowStart;

typedef struct {
  const uint8_t *ptr;
  uint32_t row;
  uint32_t col;
} LexFastPos;

typedef struct {
  TSLexTableView *v;
  const uint8_t *ptr_limit;
  uint32_t extra_bytes;
  bool utf16;
} LexUnicodeRun;

typedef struct {
  TSLexUnicodeExc exc;
  uint32_t hit_lo;
  uint32_t hit_span;
  uint16_t hit_act;
} LexUnicodeLoop;

typedef struct {
  uint32_t *cls_lo;
  uint32_t *cls_hi;
  uint32_t *min_lo;
  uint32_t *max_hi;
  uint32_t *tmpl_base;
  uint16_t *tmpl_def;
} LexUnicodeExcScratch;

static inline bool ts_lex_can_run_utf8_ascii(const Lexer *self, const TSLexTableView *v);
static inline bool ts_lex_utf8_ascii_fast(
  Lexer *self, const TSLexTableView *v, TSLexerCheckpoint *tag0, uint16_t *state_ptr, bool *result
);
static inline uint16_t ts_lex_ascii_next(
  const TSLexTableView *v, const uint16_t *sm, const uint8_t *ptr, const uint8_t *ptr_limit
);
static inline bool ts_lex_step_simple_meta(
  Lexer *self, uint16_t acc, uint16_t act, Length pos, TSLexerCheckpoint *tag0, uint32_t *flags
);
static inline uint16_t ts_lex_finish_fast_step(
  Lexer *self, uint16_t act, uint32_t start_bytes, const uint8_t *ptr_start, LexFastPos cur
);
static TS_NOINLINE bool ts_lex_run_fallback(Lexer *self, TSLexTableView *v, LexSlowStart init);
static bool ts_lex_run_loop(
  Lexer *self, TSLexTableView *v, LexSlowStart init, TSLexerCheckpoint *tags
);
static inline void ts_lex_maybe_record_dead_ends(
  Lexer *self, const TSLexTableView *v, uint16_t initial_state, Length initial_pos, bool has_result
);
static inline bool ts_lex_check_dead_end_hit(
  Lexer *self, const TSLexTableView *v, uint16_t state, uint32_t *last_block, bool has_result
);
static inline uint16_t ts_lex_next_action(
  Lexer *self, const TSLexTableView *v, TSLexStateMeta sm, bool eof
);
static inline bool ts_lex_can_run_unicode_fast(const Lexer *self, const TSLexTableView *v);
static inline int ts_lex_step_unicode_chunk(
  Lexer *self, TSLexTableView *v, uint16_t *state, LexRunState *run
);
static TS_NOINLINE bool ts_lex_unicode_fast(
  Lexer *self, TSLexTableView *v, uint16_t *state_ptr, TSLexerCheckpoint *tags, bool *result
);
static inline uint32_t ts_lex_fast_impl(
  Lexer *self, TSLexTableView *v, TSLexerCheckpoint *tags, uint16_t *state_ptr, bool utf16
);
static inline uint32_t ts_lex_fast_limit(const Lexer *self, uint32_t start_bytes);
static inline uint16_t ts_lex_fast_next(
  TSLexTableView *v, const uint16_t *sm, const uint8_t *ptr, const uint8_t *ptr_limit, bool utf16
);
static inline uint16_t ts_lex_utf8_unicode_next(
  TSLexTableView *v, const uint16_t *sm, const uint8_t *ptr, const uint8_t *ptr_limit
);
static inline uint16_t ts_lex_utf16le_next(
  TSLexTableView *v, const uint16_t *sm, const uint8_t *ptr, const uint8_t *ptr_limit
);
static inline uint16_t ts_lex_unicode_action(TSLexTableView *v, const uint16_t *sm, uint32_t cp);
static inline void ts_lex_record_simple_accept(
  Lexer *self, uint16_t acc, Length pos, const TSLexerCheckpoint *tags
);
static inline void ts_lex_commit_ascii_cursor(
  Lexer *self, Length pos, const uint8_t *ptr, uint32_t byte_delta
);
static inline void ts_lex_commit_cursor(
  Lexer *self, Length pos, const uint8_t *ptr, const uint8_t *ptr_limit, uint32_t advanced
);
static inline LexFastPos ts_lex_step_one(
  LexFastPos cur, bool utf16, uint32_t *extra_bytes
);
static inline LexFastPos ts_lex_fast_self_loop(
  LexUnicodeRun *run, const uint16_t *sm, uint16_t act, LexFastPos cur
);
static inline LexFastPos ts_lex_ascii_self_loop(
  const TSLexTableView *v, const uint16_t *sm, uint16_t act, LexFastPos cur, const uint8_t *ptr_limit
);
static inline LexFastPos ts_lex_utf16_ascii_self_loop(
  const TSLexTableView *v, const uint16_t *sm, uint16_t act, LexFastPos cur, const uint8_t *ptr_limit
);
static TS_NOINLINE LexFastPos ts_lex_unicode_self_loop(
  LexUnicodeRun *run, const uint16_t *sm, uint16_t act, LexFastPos cur
);
static inline uint32_t ts_lex_unicode_loop_step(
  LexUnicodeRun *run, const uint16_t *sm, uint16_t act, const uint8_t *ptr, LexUnicodeLoop *uni
);
static inline uint32_t ts_lex_decode_non_ascii(
  const uint8_t *ptr, const uint8_t *ptr_limit, bool utf16, uint32_t *cp_out
);
static inline LexFastPos ts_lex_step_ascii(LexFastPos cur, uint16_t ch, uint32_t step);
static inline uint16_t ts_lex_read_u16le(const uint8_t *ptr);
static inline const TSLexUnicodeExc *ts_lex_ensure_unicode_exc(TSLexTableView *v);
static void ts_lex_build_unicode_exc(TSLexTableView *v);
static void ts_lex_fill_unicode_class_bounds(const TSLexTableView *v, uint32_t *cls_lo, uint32_t *cls_hi);
static void ts_lex_collect_template_meta(const TSLexTableView *v, uint32_t *tmpl_base, uint16_t *tmpl_def);
static void ts_lex_accumulate_comb_exceptions(const TSLexTableView *v, const LexUnicodeExcScratch *s);
static inline void ts_lex_widen_bounds(uint32_t *min_lo, uint32_t *max_hi, uint32_t lo, uint32_t hi);
static inline void ts_lex_apply_accept(Lexer *self, const TSLexTableView *v, uint16_t acc, LexRunState *run);
static void ts_lex_apply_ext_accept(Lexer *self, const uint16_t *p, LexRunState *run);
static inline void ts_lex_restore_fallback_accept(Lexer *self, LexRunState *run);
static const uint16_t *ts_lex_apply_fallback_accept(Lexer *self, const uint16_t *p, LexRunState *run);
static inline bool ts_lex_check_accept_decode_error(
  const Lexer *self, uint32_t accept_start_byte, bool *out_has_decode_error
);
static inline uint16_t ts_lex_apply_transition(
  Lexer *self, const TSLexTableView *v, uint16_t act, TSLexerCheckpoint *tags
);

TS_NOINLINE TSLexTableView *ts_lex_select_view_cold(Lexer *self, const uint16_t *table) {
  if (self->kw_lex_cache.table == table) return &self->kw_lex_cache;
  TSLexTableView *slot = !self->main_lex_cache.table ? &self->main_lex_cache : &self->kw_lex_cache;
  if (slot->unicode_exc) ts_free(slot->unicode_exc);
  *slot = ts_lex_unpack_view(table);
  slot->replacement_class = slot->unicode_range_count > 0
    ? ts_lex_unicode_class(slot, 0xfffdu)
    : UINT16_MAX;
  return slot;
}

bool ts_lexer_lex_table(Lexer *self, const uint16_t *table, TSStateId start_state) {
  if (TS_UNLIKELY(!table)) return false;
  TSLexTableView *v = ts_lex_select_view(self, table);
  if (TS_UNLIKELY(start_state >= v->state_count)) return false;

  TSLexerCheckpoint tag0 = {{0, 0, 0}};
  uint16_t state = start_state;
  bool result = false;
  if (TS_LIKELY(ts_lex_can_run_utf8_ascii(self, v))) {
    Length initial_pos = self->current_position;
    if (TS_LIKELY(ts_lex_utf8_ascii_fast(self, v, &tag0, &state, &result))) return result;
    LexSlowStart init = {initial_pos, tag0, start_state, state, result};
    return ts_lex_run_fallback(self, v, init);
  }
  LexSlowStart init = {self->current_position, tag0, start_state, state, false};
  return ts_lex_run_fallback(self, v, init);
}

static inline bool ts_lex_can_run_utf8_ascii(const Lexer *self, const TSLexTableView *v) {
  return self->current_position.bytes < self->utf8_fast_limit &&
    !v->wide_base &&
    !self->has_decode_error &&
    (uint32_t)self->data.lookahead < 0x80u;
}

static inline bool ts_lex_utf8_ascii_fast(
  Lexer *self,
  const TSLexTableView *v,
  TSLexerCheckpoint *tag0,
  uint16_t *state_ptr,
  bool *result
) {
  uint16_t state = *state_ptr;
  uint32_t start_bytes = self->current_position.bytes;
  const uint8_t *ptr_start = (const uint8_t *)self->chunk + (start_bytes - self->chunk_start);
  const uint8_t *ptr_limit = (const uint8_t *)self->chunk + (self->utf8_fast_limit - self->chunk_start);
  LexFastPos cur = {ptr_start, self->current_position.extent.row, self->current_position.extent.column};
  for (;;) {
    const uint16_t *sm = &v->state_meta[(uint32_t)state * 4u];
    uint16_t acc = sm[3];
    uint16_t act = ts_lex_ascii_next(v, sm, cur.ptr, ptr_limit);
    if ((acc | act) >= TS_LEX_EXTENDED_FLAG) break;
    Length pos = {start_bytes + (uint32_t)(cur.ptr - ptr_start), {cur.row, cur.col}};
    if (acc != 0) {
      ts_lex_record_simple_accept(self, acc, pos, tag0);
      *result = true;
    }
    if (act == 0) {
      ts_lex_commit_ascii_cursor(self, pos, cur.ptr, (uint32_t)(cur.ptr - ptr_start));
      return true;
    }
    if (act & TS_LEX_SIMPLE_TAG0_FLAG) *tag0 = (TSLexerCheckpoint){{pos.bytes, cur.row, cur.col}};
    cur = ts_lex_step_ascii(cur, cur.ptr[0], 1u);
    if ((uint16_t)(act & ~TS_LEX_SIMPLE_SKIP_FLAG) == (uint16_t)(state + 1u)) {
      cur = ts_lex_ascii_self_loop(v, sm, act, cur, ptr_limit);
    }
    state = ts_lex_finish_fast_step(self, act, start_bytes, ptr_start, cur);
  }
  *state_ptr = state;
  uint32_t byte_delta = (uint32_t)(cur.ptr - ptr_start);
  Length pos = {start_bytes + byte_delta, {cur.row, cur.col}};
  ts_lex_commit_ascii_cursor(self, pos, cur.ptr, byte_delta);
  return false;
}

static inline uint16_t ts_lex_ascii_next(
  const TSLexTableView *v,
  const uint16_t *sm,
  const uint8_t *ptr,
  const uint8_t *ptr_limit
) {
  uint16_t tmpl = sm[1];
  if (tmpl == 0) return 0;
  if (TS_UNLIKELY(ptr >= ptr_limit || ptr[0] >= 0x80u)) return UINT16_MAX;
  const uint16_t *ce = &v->comb[((uint32_t)sm[0] + v->ascii_class[ptr[0]]) * 2u];
  return ce[0] == tmpl ? ce[1] : sm[2];
}

static inline bool ts_lex_step_simple_meta(
  Lexer *self,
  uint16_t acc,
  uint16_t act,
  Length pos,
  TSLexerCheckpoint *tag0,
  uint32_t *flags
) {
  if ((acc | act) >= TS_LEX_EXTENDED_FLAG) return false;
  if (acc != 0) {
    ts_lex_record_simple_accept(self, acc, pos, tag0);
    *flags |= 2u;
  }
  if (act == 0) {
    *flags |= 1u;
    return false;
  }
  if (act & TS_LEX_SIMPLE_TAG0_FLAG) *tag0 = (TSLexerCheckpoint){{pos.bytes, pos.extent.row, pos.extent.column}};
  return true;
}

static inline uint16_t ts_lex_finish_fast_step(
  Lexer *self,
  uint16_t act,
  uint32_t start_bytes,
  const uint8_t *ptr_start,
  LexFastPos cur
) {
  if (act & TS_LEX_SIMPLE_SKIP_FLAG) {
    self->token_start_position = (Length){start_bytes + (uint32_t)(cur.ptr - ptr_start), {cur.row, cur.col}};
  }
  return (act & TS_LEX_SIMPLE_STATE_MASK) - 1u;
}

static TS_NOINLINE bool ts_lex_run_fallback(Lexer *self, TSLexTableView *v, LexSlowStart init) {
  TSLexerCheckpoint stack_tags[TS_LEX_STACK_TAG_CAPACITY];
  TSLexerCheckpoint *tags = NULL;
  if (v->tag_count > TS_LEX_STACK_TAG_CAPACITY) {
    tags = ts_calloc(v->tag_count, sizeof(TSLexerCheckpoint));
    tags[0] = init.tag0;
  } else if (v->tag_count > 0) {
    stack_tags[0] = init.tag0;
    if (v->tag_count > 1u) {
      memset(stack_tags + 1, 0, (v->tag_count - 1u) * sizeof(TSLexerCheckpoint));
    }
    tags = stack_tags;
  }

  bool result = ts_lex_run_loop(self, v, init, tags);
  if (tags && tags != stack_tags) ts_free(tags);
  return result;
}

static bool ts_lex_run_loop(
  Lexer *self,
  TSLexTableView *v,
  LexSlowStart init,
  TSLexerCheckpoint *tags
) {
  LexRunState run = {{0}, tags, init.result};
  uint16_t state = init.state;
  uint32_t eof_steps = 0;
  uint32_t last_block = UINT32_MAX;
  for (;;) {
    if (TS_UNLIKELY(self->dead_ends != NULL) &&
        ts_lex_check_dead_end_hit(self, v, state, &last_block, run.result)) {
      return run.result;
    }
    int fast = ts_lex_step_unicode_chunk(self, v, &state, &run);
    if (fast > 0) break;
    if (fast < 0) continue;
    TSLexStateMeta sm = ts_lex_state_meta(v, state);
    ts_lex_apply_accept(self, v, sm.acc, &run);
    bool eof = self->data.lookahead == 0 && self->data.eof(&self->data);
    uint16_t act = ts_lex_next_action(self, v, sm, eof);
    if (act == 0 || (eof && ++eof_steps > v->state_count)) break;
    state = ts_lex_apply_transition(self, v, act, tags);
  }
  ts_lex_maybe_record_dead_ends(self, v, init.initial_state, init.initial_pos, run.result);
  return run.result;
}

static inline int ts_lex_step_unicode_chunk(
  Lexer *self,
  TSLexTableView *v,
  uint16_t *state,
  LexRunState *run
) {
  if (!ts_lex_can_run_unicode_fast(self, v)) return 0;
  uint32_t before_bytes = self->current_position.bytes;
  if (ts_lex_unicode_fast(self, v, state, run->tags, &run->result)) return 1;
  return (TS_UNLIKELY(self->dead_ends != NULL) && self->current_position.bytes != before_bytes)
    ? -1 : 0;
}

static inline void ts_lex_maybe_record_dead_ends(
  Lexer *self,
  const TSLexTableView *v,
  uint16_t initial_state,
  Length initial_pos,
  bool has_result
) {
  uint32_t anchor = has_result ? self->token_end_position.bytes : self->token_start_position.bytes;
  if (TS_UNLIKELY(!self->aborted && !self->logger.log && self->current_position.bytes > anchor &&
                  self->current_position.bytes - anchor > 256u)) {
    ts_lex_record_dead_ends(self, v, initial_state, initial_pos);
  }
}

static inline bool ts_lex_check_dead_end_hit(
  Lexer *self,
  const TSLexTableView *v,
  uint16_t state,
  uint32_t *last_block,
  bool has_result
) {
  if (self->logger.log) return false;
  uint32_t cur_byte = self->current_position.bytes;
  uint32_t block = cur_byte >> 6;
  if (block == *last_block) return false;
  *last_block = block;
  TSLexDeadEndTarget target;
  if (!ts_lex_lookup_dead_end(self->dead_ends, v->table, state, cur_byte, &target)) {
    return false;
  }
  if (!has_result) {
    ts_lexer_goto_dead_end(self, target.pos);
    if (self->track_columns) {
      self->column_data = target.col;
      self->display_column_data = target.disp;
    }
  }
  return true;
}

static inline uint16_t ts_lex_next_action(
  Lexer *self,
  const TSLexTableView *v,
  TSLexStateMeta sm,
  bool eof
) {
  if (!eof && (uint32_t)self->data.lookahead >= 0x110000u) {
    uint16_t act = ts_lex_lookup_action(v, sm, v->replacement_class);
    if (ts_lex_action_is_skip(v, act)) return 0;
    if (act != 0 && !self->has_decode_error) {
      self->has_decode_error = true;
      self->first_decode_error_byte = self->current_position.bytes;
    }
    return act;
  }
  return ts_lex_lookup_action(v, sm, ts_lex_char_class(v, self->data.lookahead, eof));
}

static inline bool ts_lex_can_run_unicode_fast(const Lexer *self, const TSLexTableView *v) {
  return !v->wide_base &&
    !self->track_columns &&
    !self->has_decode_error &&
    (self->input.encoding == TSInputEncodingUTF8 ||
     self->input.encoding == TSInputEncodingUTF16LE) &&
    !self->logger.log &&
    self->current_position.bytes < self->ascii_limit &&
    (uint32_t)self->data.lookahead < 0x110000u;
}

static TS_NOINLINE bool ts_lex_unicode_fast(
  Lexer *self,
  TSLexTableView *v,
  uint16_t *state_ptr,
  TSLexerCheckpoint *tags,
  bool *result
) {
  uint32_t flags = self->input.encoding == TSInputEncodingUTF8
    ? ts_lex_fast_impl(self, v, tags, state_ptr, false)
    : ts_lex_fast_impl(self, v, tags, state_ptr, true);
  if (flags & 2u) *result = true;
  return (flags & 1u) != 0;
}

static inline uint32_t ts_lex_fast_impl(
  Lexer *self,
  TSLexTableView *v,
  TSLexerCheckpoint *tags,
  uint16_t *state_ptr,
  bool utf16
) {
  uint16_t state = *state_ptr;
  uint32_t start_bytes = self->current_position.bytes;
  uint32_t limit_bytes = ts_lex_fast_limit(self, start_bytes);
  const uint8_t *ptr_start = (const uint8_t *)self->chunk + (start_bytes - self->chunk_start);
  const uint8_t *ptr_limit = (const uint8_t *)self->chunk + (limit_bytes - self->chunk_start);
  LexFastPos cur = {ptr_start, self->current_position.extent.row, self->current_position.extent.column};
  LexUnicodeRun u_run = {v, ptr_limit, 0, utf16};
  uint32_t flags = 0;
  for (;;) {
    const uint16_t *sm = &v->state_meta[(uint32_t)state * 4u];
    uint16_t acc = sm[3];
    uint16_t act = ts_lex_fast_next(v, sm, cur.ptr, ptr_limit, utf16);
    Length pos = {start_bytes + (uint32_t)(cur.ptr - ptr_start), {cur.row, cur.col}};
    if (!ts_lex_step_simple_meta(self, acc, act, pos, tags, &flags)) break;
    cur = ts_lex_step_one(cur, utf16, &u_run.extra_bytes);
    if ((uint16_t)(act & ~TS_LEX_SIMPLE_SKIP_FLAG) == (uint16_t)(state + 1u)) {
      cur = ts_lex_fast_self_loop(&u_run, sm, act, cur);
    }
    state = ts_lex_finish_fast_step(self, act, start_bytes, ptr_start, cur);
  }
  uint32_t byte_delta = (uint32_t)(cur.ptr - ptr_start);
  if (flags != 0 || byte_delta != 0) *state_ptr = state;
  if (byte_delta != 0) {
    Length pos = {start_bytes + byte_delta, {cur.row, cur.col}};
    uint32_t advanced = (byte_delta - u_run.extra_bytes) >> (utf16 ? 1u : 0u);
    ts_lex_commit_cursor(self, pos, cur.ptr, ptr_limit, advanced);
  }
  return flags;
}

static inline uint32_t ts_lex_fast_limit(const Lexer *self, uint32_t start_bytes) {
  uint32_t limit = self->ascii_limit;
  if (TS_UNLIKELY(self->dead_ends != NULL)) {
    uint32_t block_end = (start_bytes | 63u) + 1u;
    if (block_end > start_bytes && block_end < limit) limit = block_end;
  }
  return limit;
}

static inline uint16_t ts_lex_fast_next(
  TSLexTableView *v,
  const uint16_t *sm,
  const uint8_t *ptr,
  const uint8_t *ptr_limit,
  bool utf16
) {
  uint16_t tmpl = sm[1];
  if (tmpl == 0) return 0;
  if (!utf16) {
    if (TS_LIKELY(ptr < ptr_limit && ptr[0] < 0x80u)) {
      const uint16_t *ce = &v->comb[((uint32_t)sm[0] + v->ascii_class[ptr[0]]) * 2u];
      return ce[0] == tmpl ? ce[1] : sm[2];
    }
    return ts_lex_utf8_unicode_next(v, sm, ptr, ptr_limit);
  }
  return ts_lex_utf16le_next(v, sm, ptr, ptr_limit);
}

static inline uint16_t ts_lex_utf8_unicode_next(
  TSLexTableView *v,
  const uint16_t *sm,
  const uint8_t *ptr,
  const uint8_t *ptr_limit
) {
  if ((size_t)(ptr_limit - ptr) < 4u) return UINT16_MAX;
  int32_t cp = 0;
  ts_decode_utf8(ptr, 4u, &cp);
  return cp >= 0 ? ts_lex_unicode_action(v, sm, (uint32_t)cp) : UINT16_MAX;
}

static inline uint16_t ts_lex_utf16le_next(
  TSLexTableView *v,
  const uint16_t *sm,
  const uint8_t *ptr,
  const uint8_t *ptr_limit
) {
  if (ptr >= ptr_limit) return UINT16_MAX;
  uint16_t u = ts_lex_read_u16le(ptr);
  if (u < 0x80u) {
    const uint16_t *ce = &v->comb[((uint32_t)sm[0] + v->ascii_class[u]) * 2u];
    return ce[0] == sm[1] ? ce[1] : sm[2];
  }
  uint32_t cp = 0;
  if (!ts_lex_decode_non_ascii(ptr, ptr_limit, true, &cp)) return UINT16_MAX;
  return ts_lex_unicode_action(v, sm, cp);
}

static inline uint16_t ts_lex_unicode_action(TSLexTableView *v, const uint16_t *sm, uint32_t cp) {
  uint16_t tmpl = sm[1];
  TSLexUnicodeExc exc = ts_lex_ensure_unicode_exc(v)[tmpl];
  if (cp - exc.lo > exc.span) return sm[2];
  uint16_t cls = ts_lex_unicode_class(v, cp);
  const uint16_t *ce = &v->comb[((uint32_t)sm[0] + cls) * 2u];
  return ce[0] == tmpl ? ce[1] : sm[2];
}

static inline void ts_lex_record_simple_accept(
  Lexer *self,
  uint16_t acc,
  Length pos,
  const TSLexerCheckpoint *tags
) {
  if (acc & TS_LEX_SIMPLE_ACCEPT_TAG0_FLAG) {
    Length start = {tags[0]._opaque[0], {tags[0]._opaque[1], tags[0]._opaque[2]}};
    self->token_start_position = start;
    self->marked_start_position = start;
  }
  self->data.result_symbol = (TSSymbol)((acc & TS_LEX_SIMPLE_ACCEPT_MASK) - 1u);
  self->token_end_position = pos;
}

static inline void ts_lex_commit_ascii_cursor(
  Lexer *self,
  Length pos,
  const uint8_t *ptr,
  uint32_t byte_delta
) {
  if (byte_delta == 0) return;
  self->current_position = pos;
  self->advance_count += byte_delta;
  if (TS_UNLIKELY(self->advance_count >= 64u) && !ts_lexer_flush_operations(self)) return;
  if (TS_LIKELY(!self->has_operation_limit && ptr[0] < 0x80u)) {
    self->data.lookahead = ptr[0];
    self->lookahead_size = 1u;
  } else {
    ts_lexer_get_lookahead(self);
  }
}

static inline void ts_lex_commit_cursor(
  Lexer *self,
  Length pos,
  const uint8_t *ptr,
  const uint8_t *ptr_limit,
  uint32_t advanced
) {
  self->current_position = pos;
  self->advance_count += advanced;
  if (self->advance_count >= 64u && !ts_lexer_flush_operations(self)) return;
  if (!self->has_operation_limit) {
    if (self->input.encoding == TSInputEncodingUTF8 && ptr[0] < 0x80u) {
      self->data.lookahead = ptr[0];
      self->lookahead_size = 1u;
      return;
    }
    if (self->input.encoding == TSInputEncodingUTF16LE && ptr < ptr_limit &&
        ptr[0] < 0x80u && ptr[1] == 0) {
      self->data.lookahead = ptr[0];
      self->lookahead_size = 2u;
      return;
    }
  }
  ts_lexer_get_lookahead(self);
}

static inline LexFastPos ts_lex_step_one(
  LexFastPos cur,
  bool utf16,
  uint32_t *extra_bytes
) {
  if (!utf16) {
    uint8_t b = cur.ptr[0];
    if (TS_LIKELY(b < 0x80u)) return ts_lex_step_ascii(cur, b, 1u);
    uint32_t step = 2u + (uint32_t)(b >= 0xe0u) + (uint32_t)(b >= 0xf0u);
    cur.ptr += step;
    cur.col += step;
    *extra_bytes += step - 1u;
    return cur;
  }
  uint16_t u = ts_lex_read_u16le(cur.ptr);
  if (TS_LIKELY((u & 0xf800u) != 0xd800u)) return ts_lex_step_ascii(cur, u, 2u);
  cur.ptr += 4u;
  cur.col += 4u;
  *extra_bytes += 2u;
  return cur;
}

static inline LexFastPos ts_lex_fast_self_loop(
  LexUnicodeRun *run,
  const uint16_t *sm,
  uint16_t act,
  LexFastPos cur
) {
  cur = run->utf16
    ? ts_lex_utf16_ascii_self_loop(run->v, sm, act, cur, run->ptr_limit)
    : ts_lex_ascii_self_loop(run->v, sm, act, cur, run->ptr_limit);
  if (cur.ptr >= run->ptr_limit) return cur;
  uint16_t next_u = run->utf16 ? ts_lex_read_u16le(cur.ptr) : cur.ptr[0];
  return TS_UNLIKELY(next_u >= 0x80u) ? ts_lex_unicode_self_loop(run, sm, act, cur) : cur;
}

static inline LexFastPos ts_lex_ascii_self_loop(
  const TSLexTableView *v,
  const uint16_t *sm,
  uint16_t act,
  LexFastPos cur,
  const uint8_t *ptr_limit
) {
  const uint16_t *comb_row = &v->comb[(uint32_t)sm[0] * 2u];
  uint16_t tmpl = sm[1];
  uint16_t def = sm[2];
  while (cur.ptr < ptr_limit) {
    uint8_t b = cur.ptr[0];
    if (b >= 0x80u) break;
    const uint16_t *ce = &comb_row[(uint32_t)v->ascii_class[b] * 2u];
    if ((ce[0] == tmpl ? ce[1] : def) != act) break;
    cur = ts_lex_step_ascii(cur, b, 1u);
  }
  return cur;
}

static inline LexFastPos ts_lex_utf16_ascii_self_loop(
  const TSLexTableView *v,
  const uint16_t *sm,
  uint16_t act,
  LexFastPos cur,
  const uint8_t *ptr_limit
) {
  const uint16_t *comb_row = &v->comb[(uint32_t)sm[0] * 2u];
  uint16_t tmpl = sm[1];
  uint16_t def = sm[2];
  while (cur.ptr < ptr_limit) {
    uint16_t u = ts_lex_read_u16le(cur.ptr);
    if (u >= 0x80u) break;
    const uint16_t *ce = &comb_row[(uint32_t)v->ascii_class[u] * 2u];
    if ((ce[0] == tmpl ? ce[1] : def) != act) break;
    cur = ts_lex_step_ascii(cur, u, 2u);
  }
  return cur;
}

static TS_NOINLINE LexFastPos ts_lex_unicode_self_loop(
  LexUnicodeRun *run,
  const uint16_t *sm,
  uint16_t act,
  LexFastPos cur
) {
  TSLexTableView *v = run->v;
  const uint16_t *comb_row = &v->comb[(uint32_t)sm[0] * 2u];
  uint16_t tmpl = sm[1];
  uint16_t def = sm[2];
  uint32_t stride = run->utf16 ? 2u : 1u;
  LexUnicodeLoop uni = {
    .exc = ts_lex_ensure_unicode_exc(v)[tmpl],
    .hit_lo = 0x110000u,
    .hit_span = 0u,
    .hit_act = 0,
  };
  while (cur.ptr < run->ptr_limit) {
    uint16_t u = run->utf16 ? ts_lex_read_u16le(cur.ptr) : cur.ptr[0];
    if (u < 0x80u) {
      const uint16_t *ce = &comb_row[(uint32_t)v->ascii_class[u] * 2u];
      if ((ce[0] == tmpl ? ce[1] : def) != act) break;
      cur = ts_lex_step_ascii(cur, u, stride);
      continue;
    }
    uint32_t step = ts_lex_unicode_loop_step(run, sm, act, cur.ptr, &uni);
    if (!step) break;
    cur.ptr += step;
    cur.col += step;
    run->extra_bytes += step - stride;
  }
  return cur;
}

static inline uint32_t ts_lex_unicode_loop_step(
  LexUnicodeRun *run,
  const uint16_t *sm,
  uint16_t act,
  const uint8_t *ptr,
  LexUnicodeLoop *uni
) {
  uint32_t cp = 0;
  uint32_t step = ts_lex_decode_non_ascii(ptr, run->ptr_limit, run->utf16, &cp);
  if (!step) return 0u;
  uint16_t next_act;
  if (cp - uni->exc.lo > uni->exc.span) {
    next_act = sm[2];
  } else if (cp - uni->hit_lo <= uni->hit_span) {
    next_act = uni->hit_act;
  } else {
    uint16_t cls = ts_lex_unicode_range(run->v, cp, &uni->hit_lo, &uni->hit_span);
    const uint16_t *ce = &run->v->comb[((uint32_t)sm[0] + cls) * 2u];
    next_act = ce[0] == sm[1] ? ce[1] : sm[2];
    uni->hit_act = next_act;
  }
  return next_act == act ? step : 0u;
}

static inline uint32_t ts_lex_decode_non_ascii(
  const uint8_t *ptr,
  const uint8_t *ptr_limit,
  bool utf16,
  uint32_t *cp_out
) {
  if (!utf16) {
    if ((size_t)(ptr_limit - ptr) < 4u) return 0u;
    int32_t cp = 0;
    uint32_t step = ts_decode_utf8(ptr, 4u, &cp);
    if (cp < 0) return 0u;
    *cp_out = (uint32_t)cp;
    return step;
  }
  uint16_t u = ts_lex_read_u16le(ptr);
  if ((u & 0xf800u) != 0xd800u) {
    *cp_out = u;
    return 2u;
  }
  if ((size_t)(ptr_limit - ptr) < 4u || !U16_IS_LEAD(u)) return 0u;
  uint16_t u2 = ts_lex_read_u16le(ptr + 2u);
  if (!U16_IS_TRAIL(u2)) return 0u;
  *cp_out = (uint32_t)U16_GET_SUPPLEMENTARY(u, u2);
  return 4u;
}

static inline LexFastPos ts_lex_step_ascii(LexFastPos cur, uint16_t ch, uint32_t step) {
  if (ch == '\n') {
    cur.row++;
    cur.col = 0;
  } else {
    cur.col += step;
  }
  cur.ptr += step;
  return cur;
}

static inline uint16_t ts_lex_read_u16le(const uint8_t *ptr) {
  return (uint16_t)ptr[0] | ((uint16_t)ptr[1] << 8);
}

static inline const TSLexUnicodeExc *ts_lex_ensure_unicode_exc(TSLexTableView *v) {
  if (!v->unicode_exc) ts_lex_build_unicode_exc(v);
  return v->unicode_exc;
}

static void ts_lex_build_unicode_exc(TSLexTableView *v) {
  uint32_t tmpl_n = v->template_count;
  uint32_t cls_n = v->class_count;
  TSLexUnicodeExc *exc = ts_malloc((size_t)tmpl_n * sizeof(TSLexUnicodeExc));
  size_t scratch_bytes = ((size_t)cls_n * 2u + (size_t)tmpl_n * 4u) * sizeof(uint32_t);
  uint32_t *buf = ts_malloc(scratch_bytes);
  LexUnicodeExcScratch s = {
    .cls_lo = buf,
    .cls_hi = buf + cls_n,
    .min_lo = buf + cls_n * 2u,
    .max_hi = buf + cls_n * 2u + tmpl_n,
    .tmpl_base = buf + cls_n * 2u + tmpl_n * 2u,
    .tmpl_def = (uint16_t *)(buf + cls_n * 2u + tmpl_n * 3u),
  };

  ts_lex_fill_unicode_class_bounds(v, s.cls_lo, s.cls_hi);
  for (uint32_t t = 0; t < tmpl_n; t++) {
    s.min_lo[t] = 0x110000u;
    s.max_hi[t] = 0u;
    s.tmpl_base[t] = UINT32_MAX;
    s.tmpl_def[t] = 0;
  }
  ts_lex_collect_template_meta(v, s.tmpl_base, s.tmpl_def);
  ts_lex_accumulate_comb_exceptions(v, &s);

  for (uint32_t t = 0; t < tmpl_n; t++) {
    exc[t] = s.min_lo[t] <= s.max_hi[t]
      ? (TSLexUnicodeExc){s.min_lo[t], s.max_hi[t] - s.min_lo[t]}
      : (TSLexUnicodeExc){0x110000u, 0u};
  }
  ts_free(buf);
  v->unicode_exc = exc;
}

static void ts_lex_fill_unicode_class_bounds(
  const TSLexTableView *v,
  uint32_t *cls_lo,
  uint32_t *cls_hi
) {
  for (uint32_t c = 0; c < v->class_count; c++) {
    cls_lo[c] = 0x110000u;
    cls_hi[c] = 0u;
  }
  uint32_t start = 0x80u;
  for (uint32_t i = 0; i < v->unicode_range_count; i++) {
    uint32_t end = ts_lex_read_u32(&v->unicode_ends[i * 2u]);
    uint16_t cls = v->unicode_classes[i];
    if (start < cls_lo[cls]) cls_lo[cls] = start;
    cls_hi[cls] = end;
    start = end + 1u;
  }
}

static void ts_lex_collect_template_meta(
  const TSLexTableView *v,
  uint32_t *tmpl_base,
  uint16_t *tmpl_def
) {
  for (uint32_t s = 0; s < v->state_count; s++) {
    TSLexStateMeta sm = ts_lex_state_meta(v, (uint16_t)s);
    if (sm.tmpl == 0) continue;
    if (tmpl_base[sm.tmpl] == UINT32_MAX) {
      tmpl_base[sm.tmpl] = sm.base;
      tmpl_def[sm.tmpl] = sm.def;
    } else if (tmpl_base[sm.tmpl] != sm.base || tmpl_def[sm.tmpl] != sm.def) {
      tmpl_base[sm.tmpl] = UINT32_MAX - 1u;
    }
  }
}

static void ts_lex_accumulate_comb_exceptions(
  const TSLexTableView *v,
  const LexUnicodeExcScratch *s
) {
  for (uint32_t i = 0; i < v->comb_len; i++) {
    uint16_t t = v->comb[i * 2u];
    if (t == 0 || t >= v->template_count) continue;
    uint32_t base = s->tmpl_base[t];
    if (base == UINT32_MAX - 1u) {
      ts_lex_widen_bounds(&s->min_lo[t], &s->max_hi[t], 0x80u, 0x10ffffu);
      continue;
    }
    if (base == UINT32_MAX || i < base) continue;
    uint32_t cls = i - base;
    if (cls < v->class_count && v->comb[i * 2u + 1u] != s->tmpl_def[t]) {
      ts_lex_widen_bounds(&s->min_lo[t], &s->max_hi[t], s->cls_lo[cls], s->cls_hi[cls]);
    }
  }
}

static inline void ts_lex_widen_bounds(uint32_t *min_lo, uint32_t *max_hi, uint32_t lo, uint32_t hi) {
  if (lo > hi) return;
  if (lo < *min_lo) *min_lo = lo;
  if (hi > *max_hi) *max_hi = hi;
}

static inline void ts_lex_apply_accept(
  Lexer *self,
  const TSLexTableView *v,
  uint16_t acc,
  LexRunState *run
) {
  if (acc == 0) return;
  if (acc < TS_LEX_EXTENDED_FLAG) {
    uint32_t start_byte = (acc & TS_LEX_SIMPLE_ACCEPT_TAG0_FLAG)
      ? run->tags[0]._opaque[0]
      : self->token_start_position.bytes;
    bool has_err = false;
    if (!ts_lex_check_accept_decode_error(self, start_byte, &has_err)) return;
    if (acc & TS_LEX_SIMPLE_ACCEPT_TAG0_FLAG) {
      self->data.mark_start(&self->data, &run->tags[0]);
    }
    run->result = true;
    self->data.result_symbol = (TSSymbol)((acc & TS_LEX_SIMPLE_ACCEPT_MASK) - 1u);
    self->data.mark_end(&self->data);
    self->token_has_decode_error = has_err;
    return;
  }
  ts_lex_apply_ext_accept(self, &v->ext_accepts[acc & 0x7fffu], run);
}

static void ts_lex_apply_ext_accept(
  Lexer *self,
  const uint16_t *p,
  LexRunState *run
) {
  uint16_t flags = *p++;
  if (flags & 1u) p = ts_lex_apply_fallback_accept(self, p, run);
  if (flags & 2u) {
    ts_lex_restore_fallback_accept(self, run);
    return;
  }
  if (!(flags & 4u)) return;
  TSSymbol sym = *p++;
  bool allowed = true;
  if (flags & 8u) {
    int32_t prec = (int32_t)ts_lex_read_u32(p);
    p += 2;
    allowed = !run->fb.valid || prec >= run->fb.precedence;
  }
  uint16_t tag = (flags & 16u) ? *p : UINT16_MAX;
  uint32_t start_byte = tag != UINT16_MAX ? run->tags[tag]._opaque[0] : self->token_start_position.bytes;
  bool has_err = false;
  if (!allowed || !ts_lex_check_accept_decode_error(self, start_byte, &has_err)) return;
  if (tag != UINT16_MAX) self->data.mark_start(&self->data, &run->tags[tag]);
  run->result = true;
  self->data.result_symbol = sym;
  self->data.mark_end(&self->data);
  self->token_has_decode_error = has_err;
}

static inline void ts_lex_restore_fallback_accept(Lexer *self, LexRunState *run) {
  run->result = run->fb.valid;
  if (!run->result) {
    self->token_has_decode_error = false;
    return;
  }
  self->data.result_symbol = run->fb.symbol;
  self->data.mark_start(&self->data, &run->fb.start);
  self->data.mark_end_at(&self->data, &run->fb.end);
  self->token_has_decode_error = run->fb.has_decode_error;
}

static const uint16_t *ts_lex_apply_fallback_accept(
  Lexer *self,
  const uint16_t *p,
  LexRunState *run
) {
  TSSymbol sym = *p++;
  int32_t prec = (int32_t)ts_lex_read_u32(p);
  uint16_t tag = p[2];
  uint32_t start_byte = tag != UINT16_MAX ? run->tags[tag]._opaque[0] : self->token_start_position.bytes;
  bool has_err = false;
  if ((!run->fb.valid || prec >= run->fb.precedence) &&
      ts_lex_check_accept_decode_error(self, start_byte, &has_err)) {
    run->fb.valid = true;
    run->fb.has_decode_error = has_err;
    run->fb.precedence = prec;
    run->fb.symbol = sym;
    if (tag != UINT16_MAX) {
      run->fb.start = run->tags[tag];
    } else {
      run->fb.start = (TSLexerCheckpoint){{
        self->token_start_position.bytes,
        self->token_start_position.extent.row,
        self->token_start_position.extent.column,
      }};
    }
    self->data.mark_end(&self->data);
    self->data.get_mark_end_checkpoint(&self->data, &run->fb.end);
  }
  return p + 3;
}

static inline bool ts_lex_check_accept_decode_error(
  const Lexer *self,
  uint32_t accept_start_byte,
  bool *out_has_decode_error
) {
  if (!self->has_decode_error) {
    *out_has_decode_error = false;
    return true;
  }
  if (self->first_decode_error_byte < accept_start_byte) return false;
  *out_has_decode_error = true;
  return true;
}

static inline uint16_t ts_lex_apply_transition(
  Lexer *self,
  const TSLexTableView *v,
  uint16_t act,
  TSLexerCheckpoint *tags
) {
  if (act < TS_LEX_EXTENDED_FLAG) {
    if (act & TS_LEX_SIMPLE_TAG0_FLAG) self->data.get_checkpoint(&self->data, &tags[0]);
    self->data.advance(&self->data, (act & TS_LEX_SIMPLE_SKIP_FLAG) != 0);
    return (uint16_t)((act & TS_LEX_SIMPLE_STATE_MASK) - 1u);
  }
  const uint16_t *p = &v->ext_transitions[act & 0x7fffu];
  uint16_t next_state = p[0];
  bool skip = (p[1] & TS_LEX_EXTENDED_FLAG) != 0;
  uint16_t op_count = p[1] & 0x7fffu;
  p += 2;
  for (uint16_t i = 0; i < op_count; i++, p += 2) {
    if (p[1] != UINT16_MAX) {
      tags[p[0]] = tags[p[1]];
    } else {
      self->data.get_checkpoint(&self->data, &tags[p[0]]);
    }
  }
  self->data.advance(&self->data, skip);
  return next_state;
}
