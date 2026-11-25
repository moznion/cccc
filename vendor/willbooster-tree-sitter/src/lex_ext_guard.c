#include "./lexer.h"

typedef struct {
  Length eof_pos;
  Length start;
  Length marked;
  Length end;
  TSSymbol symbol;
} SavedExtEofState;

static void ts_lexer_advance_ext_guarded(TSLexer *_self, bool skip);
static void ts_lex_step_ext_guard_prefix(Lexer *self, bool skip, int32_t cur);
static void ts_lex_step_ext_guard_body(Lexer *self, bool skip);
static void ts_lexer_jump_to_ext_eof(Lexer *self, bool skip);
static inline Length ts_lex_next_ext_line_break(const Lexer *self);
static inline void ts_lex_disarm_ext_guard(Lexer *self);
static bool ts_lex_seek_ext_block_comment(Lexer *self, Length start_pos, uint32_t *comment_byte);
static void ts_lex_skip_ext_line_comment(Lexer *self);
static bool ts_lex_scan_ext_eof_suffix(
  Lexer *self, TSLexExtEofGuard *guard, uint32_t comment_byte
);
static void ts_lex_restore_ext_eof_state(Lexer *self, SavedExtEofState saved);
static inline bool ts_lex_is_ext_prefix_char(int32_t c);
static inline bool ts_lex_is_ext_line_break(int32_t c);

void ts_lexer_arm_ext_eof_guard(Lexer *self) {
  if (
    self->logger.log ||
    self->track_columns ||
    self->has_operation_limit ||
    self->current_included_range_index == self->included_range_count
  ) return;
  if (self->saved_advance == NULL) self->saved_advance = self->data.advance;
  self->data.advance = ts_lexer_advance_ext_guarded;
  self->ext_comment_phase = 0;
  self->ext_comment_byte = 0;
  self->ext_skip_target_byte = UINT32_MAX;
}

TS_NOINLINE void ts_lexer_maybe_record_ext_eof_guard(Lexer *self, Length start_pos) {
  if (
    self->aborted ||
    self->logger.log ||
    self->track_columns ||
    self->has_operation_limit ||
    self->included_range_count != 1
  ) return;

  if (++self->ext_eof_guard.probes > 8u) self->ext_eof_guard.checked = true;
  SavedExtEofState saved = {
    .eof_pos = self->current_position,
    .start = self->token_start_position,
    .marked = self->marked_start_position,
    .end = self->token_end_position,
    .symbol = self->data.result_symbol,
  };

  uint32_t comment_byte = 0;
  if (ts_lex_seek_ext_block_comment(self, start_pos, &comment_byte)) {
    if (ts_lex_scan_ext_eof_suffix(self, &self->ext_eof_guard, comment_byte)) {
      self->ext_eof_guard.checked = true;
      uint32_t min_safe = self->ext_eof_guard.min_safe_byte;
      if (!self->aborted && saved.eof_pos.bytes > min_safe && saved.eof_pos.bytes - min_safe > 256u) {
        self->ext_eof_guard.eof_pos = saved.eof_pos;
        self->ext_eof_guard.active = true;
      }
    } else {
      // The scan may have read on for another reason than this comment, so the probe is not spent on it.
      self->ext_eof_guard.probes--;
      self->ext_eof_guard.checked = false;
    }
  }
  if (!self->aborted) ts_lex_restore_ext_eof_state(self, saved);
}

static void ts_lexer_advance_ext_guarded(TSLexer *_self, bool skip) {
  Lexer *self = (Lexer *)_self;
  if (TS_UNLIKELY(self->track_columns)) {
    ts_lex_disarm_ext_guard(self);
    self->data.advance(&self->data, skip);
    return;
  }
  int32_t cur = self->data.lookahead;
  switch (self->ext_comment_phase) {
    case 0:
      ts_lex_step_ext_guard_prefix(self, skip, cur);
      return;
    case 1:
      self->saved_advance(&self->data, skip);
      if (ts_lex_is_ext_line_break(cur)) self->ext_comment_phase = 0;
      return;
    case 2:
      self->saved_advance(&self->data, skip);
      self->ext_comment_phase = 3;
      return;
    default:
      ts_lex_step_ext_guard_body(self, skip);
      return;
  }
}

static void ts_lex_step_ext_guard_prefix(Lexer *self, bool skip, int32_t cur) {
  if (cur == '/') {
    uint32_t slash_byte = self->current_position.bytes;
    self->saved_advance(&self->data, skip);
    int32_t next = self->data.lookahead;
    if (next == '/') {
      self->ext_comment_phase = 1;
    } else if (next == '*' && slash_byte >= self->ext_eof_guard.min_safe_byte) {
      self->ext_comment_byte = slash_byte;
      self->ext_skip_target_byte = slash_byte + 16u;
      self->ext_comment_phase = 2;
    } else {
      ts_lex_disarm_ext_guard(self);
    }
    return;
  }
  if (ts_lex_is_ext_prefix_char(cur)) {
    self->saved_advance(&self->data, skip);
    return;
  }
  ts_lex_disarm_ext_guard(self);
  self->data.advance(&self->data, skip);
}

static void ts_lex_step_ext_guard_body(Lexer *self, bool skip) {
  if (
    !length_is_undefined(self->token_end_position) &&
    self->token_end_position.bytes > self->ext_comment_byte
  ) {
    ts_lex_disarm_ext_guard(self);
    self->data.advance(&self->data, skip);
    return;
  }
  if (self->current_position.bytes < self->ext_skip_target_byte) {
    self->saved_advance(&self->data, skip);
    return;
  }
  ts_lexer_jump_to_ext_eof(self, skip);
}

static void ts_lexer_jump_to_ext_eof(Lexer *self, bool skip) {
  Length next_lb = ts_lex_next_ext_line_break(self);
  if (!length_is_undefined(next_lb)) {
    self->advance_count++;
    if (!ts_lexer_flush_operations(self)) {
      ts_lex_disarm_ext_guard(self);
      return;
    }
    ts_lexer_goto_dead_end(self, next_lb);
    if (skip) self->token_start_position = self->current_position;
    return;
  }
  self->advance_count += 64u;
  if (!ts_lexer_flush_operations(self)) {
    ts_lex_disarm_ext_guard(self);
    return;
  }
  Length eof_pos = self->ext_eof_guard.eof_pos;
  self->current_position = eof_pos;
  if (skip) self->token_start_position = eof_pos;
  self->current_included_range_index = self->included_range_count;
  self->chunk = NULL;
  self->chunk_size = 0;
  self->chunk_start = 0;
  self->ascii_limit = 0;
  self->utf8_fast_limit = 0;
  self->data.lookahead = '\0';
  self->lookahead_size = 1;
  ts_lex_disarm_ext_guard(self);
}

static inline Length ts_lex_next_ext_line_break(const Lexer *self) {
  uint32_t cur = self->current_position.bytes;
  Length nl = self->ext_eof_guard.last_nl;
  if (!length_is_undefined(nl) && cur < nl.bytes) return nl;
  Length cr = self->ext_eof_guard.last_cr;
  if (!length_is_undefined(cr) && cur < cr.bytes) return cr;
  return LENGTH_UNDEFINED;
}

static inline void ts_lex_disarm_ext_guard(Lexer *self) {
  if (self->saved_advance != NULL) {
    self->data.advance = self->saved_advance;
    self->saved_advance = NULL;
  }
}

static bool ts_lex_seek_ext_block_comment(Lexer *self, Length start_pos, uint32_t *comment_byte) {
  ts_lexer_goto_dead_end(self, start_pos);
  uint32_t limit = start_pos.bytes + 256u;
  while (self->current_included_range_index < self->included_range_count && !self->aborted) {
    int32_t c = self->data.lookahead;
    if (ts_lex_is_ext_prefix_char(c)) {
      if (self->current_position.bytes >= limit) return false;
      self->data.advance(&self->data, false);
      continue;
    }
    if (c != '/') return false;
    uint32_t slash_byte = self->current_position.bytes;
    self->data.advance(&self->data, false);
    if (self->data.lookahead == '*') {
      self->data.advance(&self->data, false);
      *comment_byte = slash_byte;
      return true;
    }
    if (self->data.lookahead != '/') return false;
    ts_lex_skip_ext_line_comment(self);
    limit = self->current_position.bytes + 256u;
  }
  return false;
}

static void ts_lex_skip_ext_line_comment(Lexer *self) {
  while (
    self->current_included_range_index < self->included_range_count &&
    !self->aborted &&
    !ts_lex_is_ext_line_break(self->data.lookahead)
  ) {
    self->data.advance(&self->data, false);
  }
}

// Reads from inside the comment at `comment_byte` to the end of the input for the line breaks that the guarded advance
// jumps to. Returns false at the comment's end when it has one: such a comment did not hold the scanner, and the guard
// only skips comments that never end.
static bool ts_lex_scan_ext_eof_suffix(
  Lexer *self,
  TSLexExtEofGuard *guard,
  uint32_t comment_byte
) {
  Length last_nl = LENGTH_UNDEFINED;
  Length last_cr = LENGTH_UNDEFINED;
  int32_t prev = 0;
  while (self->current_included_range_index < self->included_range_count && !self->aborted) {
    int32_t cur = self->data.lookahead;
    Length pos = self->current_position;
    self->data.advance(&self->data, false);
    if (prev == '*' && cur == '/') return false;
    if (cur == '\n' || cur == 0x2028 || cur == 0x2029) last_nl = pos;
    else if (cur == '\r') last_cr = pos;
    prev = cur;
  }
  guard->min_safe_byte = comment_byte;
  guard->last_nl = last_nl;
  guard->last_cr = last_cr;
  return true;
}

static void ts_lex_restore_ext_eof_state(Lexer *self, SavedExtEofState saved) {
  self->current_position = saved.eof_pos;
  self->token_start_position = saved.start;
  self->marked_start_position = saved.marked;
  self->token_end_position = saved.end;
  self->data.result_symbol = saved.symbol;
  self->current_included_range_index = self->included_range_count;
  self->chunk = NULL;
  self->chunk_size = 0;
  self->chunk_start = 0;
  self->ascii_limit = 0;
  self->utf8_fast_limit = 0;
  self->data.lookahead = '\0';
  self->lookahead_size = 1;
}

static inline bool ts_lex_is_ext_prefix_char(int32_t c) {
  uint32_t u = (uint32_t)c;
  if (u < 0x80u) {
    return u == ' ' || (u - '\t') <= ('\r' - '\t') ||
      ((u | 32u) - 'a') < 26u || (u - '0') < 10u ||
      u == '_' || u == '$' || u == '>' || u == '?';
  }
  if (u >= 0x2000u && u <= 0x200bu) return true;
  return u == 0x85u || u == 0xa0u || u == 0x1680u || u == 0x2028u ||
    u == 0x2029u || u == 0x202fu || u == 0x205fu || u == 0x2060u ||
    u == 0x3000u || u == 0xfeffu;
}

static inline bool ts_lex_is_ext_line_break(int32_t c) {
  return c == '\n' || c == '\r' || c == 0x2028 || c == 0x2029;
}
