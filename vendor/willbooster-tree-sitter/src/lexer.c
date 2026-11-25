// code-gauge-allow-file fileNcss -- the lexer keeps its position, column, and ASCII fast-path state private to this file
#include "./length.h"
#include "./lexer.h"
#include "./ts_assert.h"
#include "./unicode.h"

#include "tree_sitter/api.h"

#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#define LOG(message, character)              \
  if (self->logger.log) {                    \
    snprintf(                                \
      self->debug_buffer,                    \
      TREE_SITTER_SERIALIZATION_BUFFER_SIZE, \
      32 <= character && character < 127 ?   \
        message " character:'%c'" :          \
        message " character:%d",             \
      character                              \
    );                                       \
    self->logger.log(                        \
      self->logger.payload,                  \
      TSLogTypeLex,                          \
      self->debug_buffer                     \
    );                                       \
  }

static const int32_t BYTE_ORDER_MARK = 0xFEFF;
static const uint32_t LEXER_ADVANCES_PER_OPERATION = 64;

const TSRange TS_DEFAULT_RANGE = {
  .start_point = {0, 0},
  .end_point = {UINT32_MAX, UINT32_MAX},
  .start_byte = 0,
  .end_byte = UINT32_MAX,
};

static void ts_lexer__set_column_data(Lexer *self, uint32_t val) {
  if (!self->track_columns) return;
  self->column_data.valid = true;
  self->column_data.valid_for_replay = true;
  self->column_data.value = val;
}

static void ts_lexer__increment_column_data(Lexer *self) {
  if (self->column_data.valid) self->column_data.value++;
}

static void ts_lexer__invalidate_column_data(Lexer *self) {
  self->display_column_data.valid = false;
  self->column_data.valid = false;
  self->column_data.value = 0;
}

static bool ts_lexer__at_position(const Lexer *self, Length position) {
  return self->current_position.bytes == position.bytes &&
    point_eq(self->current_position.extent, position.extent);
}

static ColumnCheckpoint ts_lexer__column_checkpoint(const Lexer *self) {
  return (ColumnCheckpoint) {
    .position = self->current_position,
    .column = self->column_data,
    .display_column = self->display_column_data,
    .tab_width = self->display_tab_width,
  };
}

static void ts_lexer__checkpoint_columns(Lexer *self) {
  ColumnCheckpoint checkpoint = ts_lexer__column_checkpoint(self);
  if (ts_lexer__at_position(self, self->token_start_position)) {
    self->start_column_checkpoint = checkpoint;
  }
  if (ts_lexer__at_position(self, self->token_end_position)) {
    self->end_column_checkpoint = checkpoint;
  }
}

static void ts_lexer__restore_columns(Lexer *self) {
  const ColumnCheckpoint *checkpoint = NULL;
  if (ts_lexer__at_position(self, self->start_column_checkpoint.position)) {
    checkpoint = &self->start_column_checkpoint;
  }
  if (ts_lexer__at_position(self, self->end_column_checkpoint.position)) {
    checkpoint = &self->end_column_checkpoint;
  }
  if (checkpoint) {
    self->column_data = checkpoint->column;
    if (self->display_tab_width == checkpoint->tab_width) {
      self->display_column_data = checkpoint->display_column;
    }
  }
}

static void ts_lexer__checkpoint_query_columns(Lexer *self, bool display) {
  ColumnCheckpoint *checkpoint = display
    ? &self->query_display_column_checkpoint
    : &self->query_column_checkpoint;
  *checkpoint = ts_lexer__column_checkpoint(self);
  checkpoint->column.valid &= self->column_data.valid_for_replay;
  checkpoint->display_column.valid &= self->display_column_data.valid_for_replay;
}

static void ts_lexer__clear_column_checkpoints(Lexer *self) {
  self->start_column_checkpoint.position = LENGTH_UNDEFINED;
  self->end_column_checkpoint.position = LENGTH_UNDEFINED;
  self->query_column_checkpoint.position = LENGTH_UNDEFINED;
  self->query_display_column_checkpoint.position = LENGTH_UNDEFINED;
  ts_lexer__invalidate_column_data(self);
}

static bool ts_lexer__eof(const TSLexer *_self) {
  Lexer *self = (Lexer *)_self;
  return self->current_included_range_index == self->included_range_count;
}

static inline void ts_lexer__clear_ascii_limit(Lexer *self) {
  self->ascii_limit = 0;
  self->utf8_fast_limit = 0;
}

static inline void ts_lexer__update_ascii_limit(Lexer *self) {
  uint32_t stride = self->ascii_stride;
  if (
    !stride ||
    !self->chunk ||
    !self->lookahead_size ||
    self->current_included_range_index >= self->included_range_count
  ) {
    ts_lexer__clear_ascii_limit(self);
    return;
  }

  uint32_t limit = self->included_ranges[self->current_included_range_index].end_byte;
  uint32_t chunk_end = self->chunk_size <= UINT32_MAX - self->chunk_start
    ? self->chunk_start + self->chunk_size
    : UINT32_MAX;
  if (chunk_end < limit) limit = chunk_end;

  if (self->has_operation_limit) {
    uint32_t budget = self->advance_count < LEXER_ADVANCES_PER_OPERATION
      ? (LEXER_ADVANCES_PER_OPERATION - self->advance_count) * stride
      : stride;
    if (limit > self->current_position.bytes && budget < limit - self->current_position.bytes) {
      limit = self->current_position.bytes + budget;
    }
  }

  self->ascii_limit = limit < 2 * stride ? 0 : limit - 2 * stride + 1;
  self->utf8_fast_limit = (
    stride == 1 && !self->track_columns && !self->logger.log && !self->dead_ends
  ) ? self->ascii_limit : 0;
}

static void ts_lexer__clear_chunk(Lexer *self) {
  self->chunk = NULL;
  self->chunk_size = 0;
  self->chunk_start = 0;
  ts_lexer__clear_ascii_limit(self);
}

static void ts_lexer__get_chunk(Lexer *self) {
  self->chunk_start = self->current_position.bytes;
  self->chunk = self->input.read(
    self->input.payload,
    self->current_position.bytes,
    self->current_position.extent,
    &self->chunk_size
  );
  if (!self->chunk_size) {
    self->current_included_range_index = self->included_range_count;
    self->chunk = NULL;
    ts_lexer__clear_ascii_limit(self);
  }
}

static bool ts_lexer__needs_more_character_bytes(
  const Lexer *self,
  uint32_t size,
  uint32_t character_size
) {
  if (size >= character_size) return false;
  if (self->input.encoding == TSInputEncodingUTF8) {
    return self->data.lookahead == TS_DECODE_ERROR && self->lookahead_size == size;
  }
  return size < 2 || U16_IS_LEAD(self->data.lookahead);
}

static void ts_lexer__join_split_character(
  Lexer *self,
  TSDecodeFunction decode,
  const uint8_t *chunk,
  uint32_t size,
  uint32_t character_size
) {
  uint32_t max_size =
    self->included_ranges[self->current_included_range_index].end_byte - self->current_position.bytes;
  if (character_size > max_size) character_size = max_size;
  memmove(self->lookahead_buffer, chunk, size);
  while (ts_lexer__needs_more_character_bytes(self, size, character_size)) {
    TSPoint point = self->current_position.extent;
    point.column += size;
    uint32_t bytes_read = 0;
    const char *next = self->input.read(
      self->input.payload, self->current_position.bytes + size, point, &bytes_read
    );
    if (!bytes_read) break;
    if (bytes_read > character_size - size) bytes_read = character_size - size;
    memcpy(self->lookahead_buffer + size, next, bytes_read);
    size += bytes_read;
    self->lookahead_size = decode(
      (const uint8_t *)self->lookahead_buffer, size, &self->data.lookahead
    );
  }
  // Each read may invalidate the buffer of the previous one, so the copied
  // bytes replace the chunk.
  self->chunk = self->lookahead_buffer;
  self->chunk_start = self->current_position.bytes;
  self->chunk_size = size;
}

static TSDecodeFunction ts_lexer__decode_fn(TSInput input) {
  if (input.encoding == TSInputEncodingUTF8) return ts_decode_utf8;
  if (input.encoding == TSInputEncodingUTF16LE) return ts_decode_utf16_le;
  if (input.encoding == TSInputEncodingUTF16BE) return ts_decode_utf16_be;
  return input.decode;
}

static void ts_lexer__decode_split_lookahead(
  Lexer *self,
  TSDecodeFunction decode,
  const uint8_t *chunk,
  uint32_t size
) {
  uint32_t character_size = self->input.encoding == TSInputEncodingUTF8 && U8_IS_LEAD(chunk[0])
    ? 1 + U8_COUNT_TRAIL_BYTES(chunk[0])
    : self->ascii_stride == 2 ? 4 : 0;
  if (character_size) {
    ts_lexer__join_split_character(self, decode, chunk, size, character_size);
    return;
  }
  ts_lexer__get_chunk(self);
  self->lookahead_size = decode(
    (const uint8_t *)self->chunk, self->chunk_size, &self->data.lookahead
  );
}

void ts_lexer_get_lookahead(Lexer *self) {
  uint32_t position_in_chunk = self->current_position.bytes - self->chunk_start;
  uint32_t size = self->chunk_size - position_in_chunk;

  if (size == 0) {
    self->lookahead_size = 1;
    self->data.lookahead = '\0';
    ts_lexer__clear_ascii_limit(self);
    return;
  }

  const uint8_t *chunk = (const uint8_t *)self->chunk + position_in_chunk;

  if (self->input.encoding == TSInputEncodingUTF8 && chunk[0] < 0x80) {
    self->data.lookahead = chunk[0];
    self->lookahead_size = 1;
    ts_lexer__update_ascii_limit(self);
    return;
  }

  if (
    self->input.encoding == TSInputEncodingUTF16LE &&
    size >= 2 &&
    chunk[0] < 0x80 &&
    chunk[1] == 0
  ) {
    self->data.lookahead = chunk[0];
    self->lookahead_size = 2;
    ts_lexer__update_ascii_limit(self);
    return;
  }

  TSDecodeFunction decode = ts_lexer__decode_fn(self->input);
  self->lookahead_size = decode(chunk, size, &self->data.lookahead);
  if (
    size < 4 && (
      self->data.lookahead == TS_DECODE_ERROR ||
      (self->ascii_stride == 2 && U16_IS_LEAD(self->data.lookahead))
    )
  ) {
    ts_lexer__decode_split_lookahead(self, decode, chunk, size);
  }
  if (self->data.lookahead == TS_DECODE_ERROR) {
    self->lookahead_size = 1;
  }
  ts_lexer__update_ascii_limit(self);
}

static unsigned ts_lexer__first_included_range(const Lexer *self, uint32_t byte) {
  if (self->included_range_count <= 1) return 0;
  unsigned first_range = self->current_included_range_index;
  if (
    first_range > 0 && first_range <= self->included_range_count &&
    self->included_ranges[first_range - 1].end_byte > byte
  ) {
    first_range--;
  }
  if (
    first_range < self->included_range_count &&
    self->included_ranges[first_range].end_byte > byte &&
    (first_range == 0 || self->included_ranges[first_range - 1].end_byte <= byte)
  ) {
    return first_range;
  }
  unsigned low = 0, high = self->included_range_count;
  while (low < high) {
    unsigned mid = low + (high - low) / 2;
    if (self->included_ranges[mid].end_byte <= byte) low = mid + 1;
    else high = mid;
  }
  return low;
}

static bool ts_lexer__seek_included_range(Lexer *self, unsigned first_range) {
  for (unsigned i = first_range; i < self->included_range_count; i++) {
    const TSRange *included_range = &self->included_ranges[i];
    if (
      included_range->end_byte > self->current_position.bytes &&
      included_range->end_byte > included_range->start_byte
    ) {
      if (included_range->start_byte >= self->current_position.bytes) {
        self->current_position = (Length) {
          .bytes = included_range->start_byte,
          .extent = included_range->start_point,
        };
      }
      self->current_included_range_index = i;
      return true;
    }
  }
  return false;
}

static void ts_lexer_goto(Lexer *self, Length position) {
  if (self->track_columns && position.bytes != self->current_position.bytes) {
    ts_lexer__invalidate_column_data(self);
  }

  self->current_position = position;
  unsigned first_range = ts_lexer__first_included_range(self, position.bytes);

  if (ts_lexer__seek_included_range(self, first_range)) {
    if (self->chunk && (
      self->current_position.bytes < self->chunk_start ||
      self->current_position.bytes >= self->chunk_start + self->chunk_size
    )) {
      ts_lexer__clear_chunk(self);
    }

    self->lookahead_size = 0;
    self->data.lookahead = '\0';
    ts_lexer__clear_ascii_limit(self);
  } else {
    self->current_included_range_index = self->included_range_count;
    const TSRange *last_included_range = &self->included_ranges[self->included_range_count - 1];
    self->current_position = (Length) {
      .bytes = last_included_range->end_byte,
      .extent = last_included_range->end_point,
    };
    ts_lexer__clear_chunk(self);
    self->lookahead_size = 1;
    self->data.lookahead = '\0';
  }
  if (self->track_columns) ts_lexer__restore_columns(self);
}

static TS_NOINLINE void ts_lexer__advance_display_column(Lexer *self) {
  if (!self->display_column_data.valid) return;
  uint32_t column = self->display_column_data.value;
  switch (self->data.lookahead) {
    case '\n':
    case '\r':
    case '\f':
      column = 0;
      self->display_column_data.valid_for_replay = true;
      break;
    case '\t': {
      uint32_t increment = self->display_tab_width - column % self->display_tab_width;
      column = increment > UINT32_MAX - column ? UINT32_MAX : column + increment;
      break;
    }
    default:
      if (column < UINT32_MAX && !(self->current_position.bytes == 0 && self->data.lookahead == BYTE_ORDER_MARK)) column++;
      break;
  }
  self->display_column_data.value = column;
}

static bool ts_lexer__same_line_origin(Length first, Length second) {
  return first.extent.row == second.extent.row &&
    first.extent.column <= first.bytes && second.extent.column <= second.bytes &&
    first.bytes - first.extent.column == second.bytes - second.extent.column;
}

static void ts_lexer__set_range_start(Lexer *self, const TSRange *range) {
  Length position = {range->start_byte, range->start_point};
  if (position.extent.row != self->current_position.extent.row) {
    ts_lexer__set_column_data(self, 0);
    self->display_column_data.value = 0;
  }
  if (!ts_lexer__same_line_origin(position, self->current_position)) {
    self->column_data.valid_for_replay = false;
    self->display_column_data.valid_for_replay = false;
  }
  self->current_position = position;
}

static void ts_lexer__advance_position(Lexer *self) {
  if (!self->lookahead_size) return;
  ts_lexer__advance_display_column(self);
  if (self->data.lookahead == '\n') {
    self->current_position.extent.row++;
    self->current_position.extent.column = 0;
    ts_lexer__set_column_data(self, 0);
  } else {
    bool is_bom = self->current_position.bytes == 0 &&
      self->data.lookahead == BYTE_ORDER_MARK;
    if (!is_bom) ts_lexer__increment_column_data(self);
    self->current_position.extent.column += self->lookahead_size;
  }
  self->current_position.bytes += self->lookahead_size;
}

static void ts_lexer__do_advance(Lexer *self, bool skip) {
  ts_lexer__advance_position(self);
  self->advance_count++;
  if (!ts_lexer_flush_operations(self)) return;

  const TSRange *current_range = &self->included_ranges[self->current_included_range_index];
  while (
    self->current_position.bytes >= current_range->end_byte ||
    current_range->end_byte == current_range->start_byte
  ) {
    if (self->current_included_range_index < self->included_range_count) {
      self->current_included_range_index++;
    }
    if (self->current_included_range_index < self->included_range_count) {
      current_range++;
      ts_lexer__set_range_start(self, current_range);
    } else {
      current_range = NULL;
      break;
    }
  }

  if (skip) self->token_start_position = self->current_position;

  if (current_range) {
    if (
      self->current_position.bytes < self->chunk_start ||
      self->current_position.bytes >= self->chunk_start + self->chunk_size
    ) {
      ts_lexer__get_chunk(self);
    }
    ts_lexer_get_lookahead(self);
  } else {
    ts_lexer__clear_chunk(self);
    self->data.lookahead = '\0';
    self->lookahead_size = 1;
  }
}

// Out of line so that the ASCII fast path in `ts_lexer__advance_*` needs no stack frame.
static TS_NOINLINE void ts_lexer__advance_slow(Lexer *self, bool skip) {
  if (!self->chunk) return;
  if (self->logger.log) {
    if (skip) {
      LOG("skip", self->data.lookahead)
    } else {
      LOG("consume", self->data.lookahead)
    }
  }
  ts_lexer__do_advance(self, skip);
}

static inline uint32_t ts_lexer__decode_next_ascii(
  const uint8_t *next,
  TSInputEncoding encoding
) {
  if (encoding == TSInputEncodingUTF8) return next[0];
  if (encoding == TSInputEncodingUTF16LE) return (uint32_t)next[0] | ((uint32_t)next[1] << 8);
  return ((uint32_t)next[0] << 8) | (uint32_t)next[1];
}

static inline void ts_lexer__advance_fast(
  Lexer *self,
  bool skip,
  TSInputEncoding encoding
) {
  uint32_t stride = encoding == TSInputEncodingUTF8 ? 1 : 2;
  uint32_t position = self->current_position.bytes;
  int32_t lookahead = self->data.lookahead;
  if (position >= self->ascii_limit || (uint32_t)lookahead >= 0x80) {
    ts_lexer__advance_slow(self, skip);
    return;
  }
  const uint8_t *next = (const uint8_t *)self->chunk + (position + stride - self->chunk_start);
  uint32_t character = ts_lexer__decode_next_ascii(next, encoding);
  if (character >= 0x80) {
    ts_lexer__advance_slow(self, skip);
    return;
  }
  if (self->display_column_data.valid) ts_lexer__advance_display_column(self);
  if (lookahead == '\n') {
    self->current_position.extent.row++;
    self->current_position.extent.column = 0;
    ts_lexer__set_column_data(self, 0);
  } else {
    if (self->column_data.valid) self->column_data.value++;
    self->current_position.extent.column += stride;
  }
  self->current_position.bytes = position + stride;
  if (skip) self->token_start_position = self->current_position;
  self->data.lookahead = (int32_t)character;
  self->advance_count++;
}

static void ts_lexer__advance_utf8(TSLexer *self, bool skip) {
  ts_lexer__advance_fast((Lexer *)self, skip, TSInputEncodingUTF8);
}

static void ts_lexer__advance_utf16_le(TSLexer *self, bool skip) {
  ts_lexer__advance_fast((Lexer *)self, skip, TSInputEncodingUTF16LE);
}

static void ts_lexer__advance_utf16_be(TSLexer *self, bool skip) {
  ts_lexer__advance_fast((Lexer *)self, skip, TSInputEncodingUTF16BE);
}

static void ts_lexer__advance(TSLexer *self, bool skip) {
  ts_lexer__advance_slow((Lexer *)self, skip);
}

static void ts_lexer__select_advance(Lexer *self) {
  self->data.advance =
    self->logger.log ? ts_lexer__advance :
    self->input.encoding == TSInputEncodingUTF8 ? ts_lexer__advance_utf8 :
    self->input.encoding == TSInputEncodingUTF16LE ? ts_lexer__advance_utf16_le :
    self->input.encoding == TSInputEncodingUTF16BE ? ts_lexer__advance_utf16_be :
    ts_lexer__advance;
}

static bool ts_lexer__encoding_is_native(TSInputEncoding encoding) {
  const uint16_t one = 1;
  bool little_endian = *(const uint8_t *)&one == 1;
  return encoding == TSInputEncodingUTF8 ||
    encoding == (little_endian ? TSInputEncodingUTF16LE : TSInputEncodingUTF16BE);
}

static const void *ts_lexer__peek_units(const TSLexer *_self, uint32_t *count, uint32_t *size) {
  const Lexer *self = (const Lexer *)_self;
  uint32_t position = self->current_position.bytes;
  uint32_t stride = self->ascii_stride;
  *size = stride;
  *count = 0;
  // The guarded advance and the log see characters one at a time.
  if (
    position >= self->ascii_limit ||
    !ts_lexer__encoding_is_native(self->input.encoding) ||
    self->saved_advance ||
    self->logger.log
  ) return NULL;
  // `ascii_limit` leaves the last unit of the chunk out, so the lookahead after the units is in the chunk too.
  *count = (self->ascii_limit - position + stride - 1) / stride;
  return self->chunk + (position - self->chunk_start);
}

static void ts_lexer__advance_units(TSLexer *_self, uint32_t count, uint32_t line_feeds, uint32_t tail) {
  Lexer *self = (Lexer *)_self;
  if (!count) return;
  uint32_t stride = self->ascii_stride;
  ts_assert(self->current_position.bytes < self->ascii_limit);
  ts_assert(count <= (self->ascii_limit - self->current_position.bytes + stride - 1) / stride);
  ts_assert(!line_feeds || (tail <= count && line_feeds <= count - tail));
  if (self->display_column_data.valid) {
    // Tabs and line breaks among the units move the display column as they do one character at a time.
    const char *units = self->chunk + (self->current_position.bytes - self->chunk_start);
    for (uint32_t i = 0; i < count; i++) {
      uint16_t unit = (uint8_t)units[i * stride];
      if (stride == 2) memcpy(&unit, units + i * stride, sizeof(unit));
      self->data.lookahead = unit;
      ts_lexer__advance_display_column(self);
    }
  }
  if (line_feeds) {
    self->current_position.extent.row += line_feeds;
    self->current_position.extent.column = tail * stride;
    ts_lexer__set_column_data(self, tail);
  } else {
    if (self->column_data.valid) self->column_data.value += count;
    self->current_position.extent.column += count * stride;
  }
  self->current_position.bytes += count * stride;
  self->advance_count += count;
  ts_lexer_get_lookahead(self);
}

static void ts_lexer__get_position(const TSLexer *_self, TSLexerPosition *position) {
  const Lexer *self = (const Lexer *)_self;
  *position = (TSLexerPosition) {self->current_position.bytes, self->current_position.extent.row, self->current_position.extent.column};
}

static void ts_lexer__get_checkpoint(const TSLexer *_self, TSLexerCheckpoint *checkpoint) {
  *checkpoint = (TSLexerCheckpoint) {{((const Lexer *)_self)->current_position.bytes, ((const Lexer *)_self)->current_position.extent.row, ((const Lexer *)_self)->current_position.extent.column}};
}

static void ts_lexer__mark_start(TSLexer *_self, const TSLexerCheckpoint *checkpoint) {
  ((Lexer *)_self)->token_start_position = ((Lexer *)_self)->marked_start_position =
    (Length) {checkpoint->_opaque[0], {checkpoint->_opaque[1], checkpoint->_opaque[2]}};
}

static void ts_lexer__mark_end_at(TSLexer *_self, const TSLexerCheckpoint *checkpoint) {
  ((Lexer *)_self)->token_end_position =
    (Length) {checkpoint->_opaque[0], {checkpoint->_opaque[1], checkpoint->_opaque[2]}};
  if (((Lexer *)_self)->track_columns) ts_lexer__checkpoint_columns((Lexer *)_self);
}

static void ts_lexer__get_mark_end(const TSLexer *_self, TSLexerPosition *position) {
  *position = (TSLexerPosition) {((const Lexer *)_self)->token_end_position.bytes, ((const Lexer *)_self)->token_end_position.extent.row, ((const Lexer *)_self)->token_end_position.extent.column};
}

static void ts_lexer__get_mark_end_checkpoint(const TSLexer *_self, TSLexerCheckpoint *checkpoint) {
  *checkpoint = (TSLexerCheckpoint) {{((const Lexer *)_self)->token_end_position.bytes, ((const Lexer *)_self)->token_end_position.extent.row, ((const Lexer *)_self)->token_end_position.extent.column}};
}

static void ts_lexer__mark_end(TSLexer *_self) {
  Lexer *self = (Lexer *)_self;
  if (self->current_included_range_index > 0 && !ts_lexer__eof(&self->data)) {
    const TSRange *current_included_range = &self->included_ranges[self->current_included_range_index];
    if (self->current_position.bytes == current_included_range->start_byte) {
      const TSRange *prev = current_included_range - 1;
      self->token_end_position = (Length) {prev->end_byte, prev->end_point};
      return;
    }
  }
  self->token_end_position = self->current_position;
  if (self->track_columns) {
    self->end_column_checkpoint = ts_lexer__column_checkpoint(self);
    if (ts_lexer__at_position(self, self->token_start_position)) {
      self->start_column_checkpoint = self->end_column_checkpoint;
    }
  }
}

static bool ts_lexer__restore_query_checkpoint(Lexer *self, bool display) {
  ColumnCheckpoint checkpoint = display
    ? self->query_display_column_checkpoint
    : self->query_column_checkpoint;
  Length goal = self->current_position;
  Length position = checkpoint.position;
  bool display_valid = checkpoint.display_column.valid &&
    checkpoint.tab_width == self->display_tab_width;
  bool valid = display
    ? display_valid
    : checkpoint.column.valid && (!self->display_tab_width || display_valid);
  if (
    !valid || length_is_undefined(position) || position.bytes > goal.bytes ||
    !ts_lexer__same_line_origin(position, goal)
  ) return false;

  ts_lexer_goto(self, position);
  if (!ts_lexer__at_position(self, position)) return false;
  self->column_data = checkpoint.column;
  self->display_column_data = checkpoint.display_column;
  if (checkpoint.tab_width != self->display_tab_width) self->display_column_data.valid = false;
  return true;
}

static void ts_lexer__replay_columns(Lexer *self, bool display) {
  uint32_t goal_byte = self->current_position.bytes;
  Length start = {goal_byte - self->current_position.extent.column, {self->current_position.extent.row, 0}};
  if (!ts_lexer__restore_query_checkpoint(self, display)) {
    ts_lexer_goto(self, start);
    ts_lexer__set_column_data(self, 0);
    self->display_column_data = (ColumnData) {0, self->display_tab_width != 0, true};
  }
  if (!self->chunk) ts_lexer__get_chunk(self);
  if (!ts_lexer__eof(&self->data)) {
    ts_lexer_get_lookahead(self);
    while (self->current_position.bytes < goal_byte && !ts_lexer__eof(&self->data) && self->chunk) {
      ts_lexer__do_advance(self, false);
    }
  }
}

static uint32_t ts_lexer__get_column(TSLexer *_self) {
  Lexer *self = (Lexer *)_self;
  self->track_columns = true;
  if (!self->column_data.valid) ts_lexer__replay_columns(self, false);
  ts_lexer__checkpoint_columns(self);
  ts_lexer__checkpoint_query_columns(self, false);
  return self->column_data.value;
}

static uint32_t ts_lexer__get_display_column(TSLexer *_self, uint32_t tab_width) {
  Lexer *self = (Lexer *)_self;
  self->track_columns = true;
  if (!tab_width) tab_width = 1;
  if (!self->display_column_data.valid || self->display_tab_width != tab_width) {
    ColumnData column_data = self->column_data;
    self->display_tab_width = tab_width;
    ts_lexer__replay_columns(self, true);
    self->column_data = column_data;
  }
  ts_lexer__checkpoint_columns(self);
  ts_lexer__checkpoint_query_columns(self, true);
  return self->display_column_data.value;
}

// Is the lexer at a boundary between two disjoint included ranges of
// source code? This is exposed as an API because some languages' external
// scanners need to perform custom actions at these boundaries.
static bool ts_lexer__is_at_included_range_start(const TSLexer *_self) {
  const Lexer *self = (const Lexer *)_self;
  if (self->current_included_range_index >= self->included_range_count) return false;
  const TSRange *current_range = &self->included_ranges[self->current_included_range_index];
  if (self->current_position.bytes != current_range->start_byte) return false;

  uint32_t previous = self->current_included_range_index;
  while (previous > 0 &&
         self->included_ranges[previous - 1].start_byte == self->included_ranges[previous - 1].end_byte) {
    previous--;
  }
  return previous == 0 || self->included_ranges[previous - 1].end_byte < current_range->start_byte;
}

static void ts_lexer__log(const TSLexer *_self, const char *fmt, ...) {
  Lexer *self = (Lexer *)_self;
  va_list args;
  va_start(args, fmt);
  if (self->logger.log) {
    vsnprintf(self->debug_buffer, TREE_SITTER_SERIALIZATION_BUFFER_SIZE, fmt, args);
    self->logger.log(self->logger.payload, TSLogTypeLex, self->debug_buffer);
  }
  va_end(args);
}

void ts_lexer_init(Lexer *self) {
  *self = (Lexer) {
    .data = {
      // The lexer's methods are stored as struct fields so that generated
      // parsers can call them without needing to be linked against this
      // library.
      .advance = ts_lexer__advance,
      .mark_end = ts_lexer__mark_end,
      .get_position = ts_lexer__get_position,
      .mark_start = ts_lexer__mark_start,
      .mark_end_at = ts_lexer__mark_end_at,
      .get_mark_end = ts_lexer__get_mark_end,
      .get_column = ts_lexer__get_column,
      .get_display_column = ts_lexer__get_display_column,
      .get_checkpoint = ts_lexer__get_checkpoint,
      .get_mark_end_checkpoint = ts_lexer__get_mark_end_checkpoint,
      .peek_units = ts_lexer__peek_units,
      .advance_units = ts_lexer__advance_units,
      .is_at_included_range_start = ts_lexer__is_at_included_range_start,
      .eof = ts_lexer__eof,
      .log = ts_lexer__log,
    },
  };
  ts_lexer_set_included_ranges(self, NULL, 0);
}

void ts_lexer_delete(Lexer *self) {
  ts_lexer_clear_cache(self);
  if (self->included_ranges != &TS_DEFAULT_RANGE) ts_free((void *)self->included_ranges);
}

void ts_lexer_set_input(Lexer *self, TSInput input) {
  ts_lexer_clear_dead_ends(self);
  self->input = input;
  self->ascii_stride =
    input.encoding == TSInputEncodingUTF8 ? 1 :
    input.encoding == TSInputEncodingUTF16LE || input.encoding == TSInputEncodingUTF16BE ? 2 : 0;
  self->track_columns = self->included_ranges != &TS_DEFAULT_RANGE;
  ts_lexer__select_advance(self);
  ts_lexer__clear_column_checkpoints(self);
  ts_lexer__clear_chunk(self);
  ts_lexer_goto(self, self->current_position);
}

void ts_lexer_set_logger(Lexer *self, TSLogger logger) {
  self->logger = logger;
  ts_lexer__select_advance(self);
  ts_lexer__update_ascii_limit(self);
}

void ts_lexer_set_progress_callback(
  Lexer *self,
  bool (*check_progress)(void *payload, unsigned operations),
  void *payload
) {
  self->check_progress = check_progress;
  self->progress_payload = payload;
}

void ts_lexer_reset(Lexer *self, Length position) {
  if (position.bytes != self->current_position.bytes) {
    ts_lexer_goto(self, position);
  }
}

void ts_lexer_goto_dead_end(Lexer *self, Length position) {
  ts_lexer_reset(self, position);
  if (!ts_lexer__eof(&self->data)) {
    if (!self->chunk_size) ts_lexer__get_chunk(self);
    if (!self->lookahead_size) ts_lexer_get_lookahead(self);
  }
}

void ts_lexer_start(Lexer *self) {
  self->token_start_position = self->current_position;
  self->token_end_position = LENGTH_UNDEFINED;
  self->marked_start_position = LENGTH_UNDEFINED;
  self->data.result_symbol = 0;
  self->aborted = false;
  self->has_decode_error = false;
  self->token_has_decode_error = false;
  if (!ts_lexer__eof(&self->data)) {
    if (!self->chunk_size) ts_lexer__get_chunk(self);
    if (!self->lookahead_size) ts_lexer_get_lookahead(self);
    if (self->current_position.bytes == 0) {
      if (self->data.lookahead == BYTE_ORDER_MARK) {
        ts_lexer__advance_slow(self, true);
      }
      if (self->track_columns) ts_lexer__set_column_data(self, 0);
    }
  }
  if (self->track_columns) {
    self->start_column_checkpoint = ts_lexer__column_checkpoint(self);
    self->end_column_checkpoint.position = LENGTH_UNDEFINED;
  }
}

void ts_lexer_finish(Lexer *self) {
  if (!self->has_operation_limit) ts_lexer_flush_operations(self);
  if (!length_is_undefined(self->marked_start_position)) {
    self->token_start_position = self->marked_start_position;
  }
  if (length_is_undefined(self->token_end_position)) ts_lexer__mark_end(&self->data);
  if (self->token_end_position.bytes < self->token_start_position.bytes) {
    self->token_start_position = self->token_end_position;
  }
}

bool ts_lexer_flush_operations(Lexer *self) {
  if (self->advance_count < LEXER_ADVANCES_PER_OPERATION) return true;
  unsigned operations = self->advance_count / LEXER_ADVANCES_PER_OPERATION;
  self->advance_count %= LEXER_ADVANCES_PER_OPERATION;
  if (!self->check_progress || self->check_progress(self->progress_payload, operations)) {
    return true;
  }
  self->aborted = true;
  self->current_included_range_index = self->included_range_count;
  ts_lexer__clear_chunk(self);
  self->data.lookahead = '\0';
  self->lookahead_size = 1;
  return false;
}

void ts_lexer_mark_end(Lexer *self) {
  ts_lexer__mark_end(&self->data);
}

static bool ts_lexer__ranges_are_valid(const TSRange *ranges, uint32_t count) {
  uint32_t previous_byte = 0;
  for (uint32_t i = 0; i < count; i++) {
    if (ranges[i].start_byte < previous_byte || ranges[i].end_byte < ranges[i].start_byte) {
      return false;
    }
    previous_byte = ranges[i].end_byte;
  }
  return true;
}

bool ts_lexer_set_included_ranges(
  Lexer *self,
  const TSRange *ranges,
  uint32_t count
) {
  if (count == 0 || !ranges) {
    ranges = &TS_DEFAULT_RANGE;
    count = 1;
  } else if (!ts_lexer__ranges_are_valid(ranges, count)) {
    return false;
  }

  if (count == 1 && (ranges == &TS_DEFAULT_RANGE || memcmp(ranges, &TS_DEFAULT_RANGE, sizeof(TSRange)) == 0)) {
    if (self->included_ranges != &TS_DEFAULT_RANGE) {
      ts_free((void *)self->included_ranges);
      self->included_ranges = &TS_DEFAULT_RANGE;
    }
  } else {
    size_t size = count * sizeof(TSRange);
    TSRange *allocated = self->included_ranges == &TS_DEFAULT_RANGE
      ? ts_malloc(size)
      : ts_realloc((void *)self->included_ranges, size);
    memcpy(allocated, ranges, size);
    self->included_ranges = allocated;
  }
  self->included_range_count = count;
  self->track_columns = self->included_ranges != &TS_DEFAULT_RANGE;
  ts_lexer_clear_dead_ends(self);
  ts_lexer__clear_column_checkpoints(self);
  ts_lexer_goto(self, self->current_position);
  return true;
}

const TSRange *ts_lexer_included_ranges(const Lexer *self, uint32_t *count) {
  *count = self->included_range_count;
  return self->included_ranges;
}

#undef LOG
