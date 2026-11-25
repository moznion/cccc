#ifndef TREE_SITTER_LEXER_H_
#define TREE_SITTER_LEXER_H_

#ifdef __cplusplus
extern "C" {
#endif

#include "./length.h"
#include "./subtree.h"
#include "tree_sitter/api.h"
#include "./parser.h"

#ifndef TS_NOINLINE
#if defined _WIN32 && !defined __GNUC__
#define TS_NOINLINE __declspec(noinline)
#else
#define TS_NOINLINE __attribute__((noinline))
#endif
#endif

#ifndef TS_LIKELY
#if defined(__GNUC__) || defined(__clang__)
#define TS_LIKELY(x) __builtin_expect(!!(x), 1)
#define TS_UNLIKELY(x) __builtin_expect(!!(x), 0)
#else
#define TS_LIKELY(x) (x)
#define TS_UNLIKELY(x) (x)
#endif
#endif

typedef struct TSLexDeadEndCache TSLexDeadEndCache;

extern const TSRange TS_DEFAULT_RANGE;

typedef struct {
  uint32_t value;
  bool valid;
  bool valid_for_replay;
} ColumnData;

typedef struct {
  Length position;
  ColumnData column;
  ColumnData display_column;
  uint32_t tab_width;
} ColumnCheckpoint;

typedef struct {
  uint32_t lo;
  uint32_t span;
} TSLexUnicodeExc;

typedef struct {
  const uint16_t *table;
  TSLexUnicodeExc *unicode_exc;
  uint32_t total_words;
  uint16_t state_count;
  uint16_t template_count;
  uint16_t class_count;
  uint16_t unicode_range_count;
  uint16_t tag_count;
  uint16_t replacement_class;
  bool wide_base;
  bool direct_keyword_lookup;
  uint32_t comb_len;
  uint32_t ext_transitions_len;
  uint32_t ext_accepts_len;
  const uint16_t *ascii_class;
  const uint16_t *unicode_ends;
  const uint16_t *unicode_classes;
  const uint16_t *state_meta;
  const uint16_t *comb;
  const uint16_t *ext_transitions;
  const uint16_t *ext_accepts;
} TSLexTableView;

typedef struct {
  Length eof_pos;
  Length last_nl;
  Length last_cr;
  uint32_t min_safe_byte;
  uint8_t probes;
  bool checked;
  bool active;
} TSLexExtEofGuard;

typedef struct {
  Length token_start;
  Length token_end;
  uint32_t start_byte;
  uint32_t scanner_state_id;
  uint32_t new_scanner_state_id;
  uint16_t external_lex_state;
  TSSymbol result_symbol;
  bool found_token;
  bool valid;
} TSLexExtScanCache;

typedef struct {
  TSLexer data;
  Length current_position;
  Length token_start_position;
  Length marked_start_position;
  Length token_end_position;

  const TSRange *included_ranges;
  const char *chunk;
  TSInput input;
  TSLogger logger;

  uint32_t included_range_count;
  uint32_t current_included_range_index;
  uint32_t chunk_start;
  uint32_t chunk_size;
  uint32_t lookahead_size;
  uint32_t ascii_stride;
  uint32_t ascii_limit;
  uint32_t utf8_fast_limit;
  uint32_t advance_count;
  uint32_t first_decode_error_byte;
  bool track_columns;
  bool has_operation_limit;
  bool aborted;
  bool has_decode_error;
  bool token_has_decode_error;
  bool (*check_progress)(void *payload, unsigned operations);
  void *progress_payload;
  void (*saved_advance)(TSLexer *, bool);
  uint32_t ext_comment_byte;
  uint32_t ext_skip_target_byte;
  uint8_t ext_comment_phase;
  char lookahead_buffer[4];
  ColumnData column_data;
  ColumnData display_column_data;
  uint32_t display_tab_width;
  ColumnCheckpoint start_column_checkpoint;
  ColumnCheckpoint end_column_checkpoint;
  ColumnCheckpoint query_column_checkpoint;
  ColumnCheckpoint query_display_column_checkpoint;
  TSLexDeadEndCache *dead_ends;
  TSLexExtEofGuard ext_eof_guard;
  TSLexExtScanCache ext_scan_cache[2];
  TSLexTableView main_lex_cache;
  TSLexTableView kw_lex_cache;

  char debug_buffer[TREE_SITTER_SERIALIZATION_BUFFER_SIZE];
} Lexer;

void ts_lexer_init(Lexer *self);
void ts_lexer_delete(Lexer *self);
void ts_lexer_clear_cache(Lexer *self);
void ts_lexer_clear_dead_ends(Lexer *self);
void ts_lexer_goto_dead_end(Lexer *self, Length position);
void ts_lexer_arm_ext_eof_guard(Lexer *self);
void ts_lexer_maybe_record_ext_eof_guard(Lexer *self, Length start_pos);
void ts_lexer_set_input(Lexer *self, TSInput input);
void ts_lexer_set_logger(Lexer *self, TSLogger logger);
void ts_lexer_set_progress_callback(
  Lexer *self,
  bool (*check_progress)(void *payload, unsigned operations),
  void *payload
);
void ts_lexer_reset(Lexer *self, Length position);
void ts_lexer_start(Lexer *self);
void ts_lexer_finish(Lexer *self);
void ts_lexer_get_lookahead(Lexer *self);
bool ts_lexer_flush_operations(Lexer *self);
void ts_lexer_mark_end(Lexer *self);
bool ts_lexer_set_included_ranges(Lexer *self, const TSRange *ranges, uint32_t count);
const TSRange *ts_lexer_included_ranges(const Lexer *self, uint32_t *count);

static inline void ts_lexer_reset_fast(Lexer *self, Length position) {
  if (TS_UNLIKELY(position.bytes != self->current_position.bytes)) {
    ts_lexer_reset(self, position);
  }
}

static inline void ts_lexer_start_fast(Lexer *self) {
  if (TS_LIKELY(
    self->current_position.bytes < self->utf8_fast_limit &&
    (uint32_t)self->data.lookahead < 0x80u
  )) {
    self->token_start_position = self->current_position;
    self->token_end_position = LENGTH_UNDEFINED;
    self->marked_start_position = LENGTH_UNDEFINED;
    self->data.result_symbol = 0;
    self->aborted = false;
    self->has_decode_error = false;
    self->token_has_decode_error = false;
    return;
  }
  ts_lexer_start(self);
}

static inline void ts_lexer_finish_fast(Lexer *self) {
  if (TS_UNLIKELY(!self->has_operation_limit && self->advance_count >= 64u)) {
    ts_lexer_flush_operations(self);
  }
  if (TS_UNLIKELY(!length_is_undefined(self->marked_start_position))) {
    self->token_start_position = self->marked_start_position;
  }
  if (TS_UNLIKELY(length_is_undefined(self->token_end_position))) {
    ts_lexer_mark_end(self);
  }
  if (TS_UNLIKELY(self->token_end_position.bytes < self->token_start_position.bytes)) {
    self->token_start_position = self->token_end_position;
  }
}

static inline void ts_lexer_begin_external_scan(Lexer *self) {
  if (TS_UNLIKELY(self->ext_eof_guard.active)) {
    ts_lexer_arm_ext_eof_guard(self);
  }
}

static inline void ts_lexer_end_external_scan(Lexer *self, Length start_pos, bool found_token) {
  if (TS_UNLIKELY(self->saved_advance != NULL)) {
    self->data.advance = self->saved_advance;
    self->saved_advance = NULL;
  } else if (TS_UNLIKELY(
    !self->ext_eof_guard.checked &&
    self->current_included_range_index == self->included_range_count &&
    self->current_position.bytes > start_pos.bytes &&
    self->current_position.bytes - start_pos.bytes > 256u &&
    (!found_token || (
      !length_is_undefined(self->token_end_position) &&
      self->token_end_position.bytes <= start_pos.bytes
    ))
  )) {
    ts_lexer_maybe_record_ext_eof_guard(self, start_pos);
  }
}

#ifdef __cplusplus
}
#endif

#endif  // TREE_SITTER_LEXER_H_
