#ifndef TREE_SITTER_LANGUAGE_H_
#define TREE_SITTER_LANGUAGE_H_

#ifdef __cplusplus
extern "C" {
#endif

#include "./subtree.h"
#include "./parser.h"

#define ts_builtin_sym_error_repeat (ts_builtin_sym_error - 1)

// Each small state has one `small_parse_table_map` entry for its terminal row. Nonterminal gotos for all
// states start `small_parse_table`: per-nonterminal defaults, then a 64-state directory bitmap, then the
// 32-bit map entries of non-empty goto rows. Bits 29-31 of a map entry identify its encoding: grouped (`0`),
// descriptor (`SMALL_STATE_DESCRIPTOR_FLAG`), 64-bit bitmap (`SMALL_STATE_BITMAP_FLAG`), default-only goto
// set (`SMALL_STATE_DEFAULT_GOTO_FLAG`), sorted pairs (`SMALL_STATE_PAIR_FLAG`), or shared symbol sets
// (`SMALL_STATE_SET_FLAG`).
#define SMALL_STATE_DESCRIPTOR_FLAG 0x20000000u
#define SMALL_STATE_BASE_FLAG SMALL_STATE_DESCRIPTOR_FLAG
#define SMALL_STATE_BITMAP_FLAG 0x40000000u
#define SMALL_STATE_DEFAULT_GOTO_FLAG (SMALL_STATE_DESCRIPTOR_FLAG | SMALL_STATE_BITMAP_FLAG)
#define SMALL_STATE_PAIR_FLAG 0x80000000u
#define SMALL_STATE_SET_FLAG (SMALL_STATE_BITMAP_FLAG | SMALL_STATE_PAIR_FLAG)
#define SMALL_STATE_ENCODING_MASK 0xe0000000u
#define SMALL_STATE_OFFSET_MASK 0x1fffffffu

typedef struct {
  const uint32_t *actions;
  uint32_t action_count;
  bool is_reusable;
} TableEntry;

typedef enum {
  LookaheadFresh,      // no `next()` yet
  LookaheadPositioned, // last `next()` returned true
  LookaheadDone,       // last `next()` returned false
} LookaheadPhase;

typedef struct {
  const TSLanguage *language;
  const uint16_t *data;
  const uint16_t *group_end;
  const uint16_t *set_data;
  uint64_t bitmap_bits;
  uint32_t set_bits;
  uint32_t set_symbol;
  uint16_t table_value;
  uint16_t group_count;
  const uint16_t *descriptor;
  bool is_small_state;
  bool is_pair_state;
  bool is_bitmap_state;
  bool is_set_state;
  TSStateId state;
  LookaheadPhase phase;

  const uint32_t *actions;
  TSSymbol symbol;
  uint16_t action_count;
} LookaheadIterator;

// Languages with another ABI version may lay out `TSLanguage` differently after `abi_version`,
// so accessors called before a parser accepts a language must check `ts_language_has_supported_abi` first.
static inline bool ts_language_abi_version_is_supported(uint32_t abi_version) {
  return abi_version >= TREE_SITTER_MIN_COMPATIBLE_LANGUAGE_VERSION && abi_version <= TREE_SITTER_LANGUAGE_VERSION;
}

static inline bool ts_language_has_supported_abi(const TSLanguage *self) {
  return ts_language_abi_version_is_supported(self->abi_version);
}

static inline uint32_t ts_language__trailing_zeros(uint32_t bits);
static inline uint32_t ts_language__count_ones64(uint64_t bits);
static inline uint32_t ts_language__trailing_zeros64(uint64_t bits);

uint16_t ts_language_lookup_small_compact(const TSLanguage *self, uint32_t index, TSSymbol symbol);

static inline uint32_t ts_language__small_offset(const uint16_t *words) {
  return words[0] | (uint32_t)words[1] << 16;
}

// Whether the shared symbol set at `offset` in `small_parse_table` contains `symbol`.
static inline bool ts_language_small_set_contains(const TSLanguage *self, uint32_t offset, TSSymbol symbol) {
  const uint16_t *set = &self->small_parse_table[offset];
  uint32_t word = symbol / 16u - set[0];
  return word < set[1] && (set[word + 2] & (1u << (symbol % 16)));
}

// Look up a symbol that the set of a descriptor contains.
static inline uint16_t ts_language_lookup_described(const TSLanguage *self, const uint16_t *descriptor, TSSymbol symbol) {
  uint32_t diff = ts_language__small_offset(descriptor + 2);
  uint16_t value = ts_language_lookup_small_compact(self, diff & ~SMALL_STATE_BASE_FLAG, symbol);
  if (!value && (diff & SMALL_STATE_BASE_FLAG)) {
    value = ts_language_lookup_small_compact(self, ts_language__small_offset(descriptor + 4), symbol);
  }
  if (value || symbol < self->token_count) return value;
  return self->small_parse_table[symbol - self->token_count];
}

// Look up a symbol through a descriptor located by a map entry with `SMALL_STATE_DESCRIPTOR_FLAG`.
static inline uint16_t ts_language_lookup_with_descriptor(const TSLanguage *self, uint32_t index, TSSymbol symbol) {
  const uint16_t *descriptor = &self->small_parse_table[index & SMALL_STATE_OFFSET_MASK];
  if (!ts_language_small_set_contains(self, ts_language__small_offset(descriptor), symbol)) return 0;
  return ts_language_lookup_described(self, descriptor, symbol);
}

static inline uint16_t ts_language_lookup_small_row(const TSLanguage *self, uint32_t index, TSSymbol symbol) {
  uint32_t encoding = index & SMALL_STATE_ENCODING_MASK;
  if (encoding == SMALL_STATE_DEFAULT_GOTO_FLAG) {
    return ts_language_small_set_contains(self, index & SMALL_STATE_OFFSET_MASK, symbol)
      ? self->small_parse_table[symbol - self->token_count]
      : 0;
  }
  if (encoding == SMALL_STATE_DESCRIPTOR_FLAG) {
    return ts_language_lookup_with_descriptor(self, index, symbol);
  }
  return ts_language_lookup_small_compact(self, index, symbol);
}

void ts_language_table_entry(const TSLanguage *self, TSStateId state, TSSymbol symbol, TableEntry *result);
TSLexerMode ts_language_lex_mode_for_state(const TSLanguage *self, TSStateId state);
bool ts_language_is_reserved_word(const TSLanguage *self, TSStateId state, TSSymbol symbol);
TSSymbolMetadata ts_language_symbol_metadata(const TSLanguage *self, TSSymbol symbol);
TSSymbol ts_language_public_symbol(const TSLanguage *self, TSSymbol symbol);
const TSLanguage *ts_language_copy_without_callbacks(const TSLanguage *self);
#ifdef __wasm__
uint32_t ts_language_current_context_id(void);
#endif

static inline const uint32_t *ts_language_actions(
  const TSLanguage *self,
  TSStateId state,
  TSSymbol symbol,
  uint32_t *count
) {
  TableEntry entry;
  ts_language_table_entry(self, state, symbol, &entry);
  *count = entry.action_count;
  return entry.actions;
}

static inline bool ts_parse_action_is_reduce(const TSParseAction *action) {
  return action->type == TSParseActionTypeReduce || action->type == TSParseActionTypeReduceExtra;
}

static inline bool ts_packed_parse_action_is_reduce(uint32_t action) {
  uint32_t type = action & TS_PARSE_ACTION_TYPE_MASK;
  return type == TSParseActionTypeReduce || type == TSParseActionTypeReduceExtra;
}

// Decode an action packed as `parser.h` describes.
static inline TSParseAction ts_language_parse_action(const TSLanguage *self, uint32_t action) {
  TSParseAction result = {.type = action & TS_PARSE_ACTION_TYPE_MASK};
  if (ts_packed_parse_action_is_reduce(action)) {
    const uint32_t *record = &self->parse_actions[(action >> 8) & 0xffff];
    result.reduce.symbol = record[0] & 0xffff;
    result.reduce.child_count = (record[0] >> 16) & 0xff;
    result.reduce.dynamic_precedence = (int16_t)(record[1] & 0xffff);
    result.reduce.production_id = record[1] >> 16;
  } else {
    result.shift.state = (action >> 8) & 0xffff;
    result.shift.extra = (action & TS_PARSE_ACTION_EXTRA) != 0;
  }
  return result;
}

static inline bool ts_table_entry_has_reduce_action(const TableEntry *entry) {
  for (unsigned i = 0; i < entry->action_count; i++) {
    if (ts_packed_parse_action_is_reduce(entry->actions[i])) return true;
  }
  return false;
}

static inline bool ts_language_has_reduce_action(
  const TSLanguage *self,
  TSStateId state,
  TSSymbol symbol
) {
  TableEntry entry;
  ts_language_table_entry(self, state, symbol, &entry);
  return ts_table_entry_has_reduce_action(&entry);
}

static inline uint16_t ts_language_lex_mode_id(const TSLanguage *self, uint32_t state) {
  return self->lex_mode_count <= UINT8_MAX + 1u
    ? ((const uint8_t *)self->lex_mode_ids)[state]
    : ((const uint16_t *)self->lex_mode_ids)[state];
}

// Look up the terminal parse-table cell for `(state, symbol)`.
static inline uint16_t ts_language_lookup(
  const TSLanguage *self,
  TSStateId state,
  TSSymbol symbol
) {
  if (symbol >= self->token_count) return 0;
  if (state >= self->large_state_count) {
    uint32_t index = self->small_parse_table_map[state - self->large_state_count];
    return ts_language_lookup_small_row(self, index, symbol);
  }
  return self->parse_table[(uint32_t)state * self->token_count + symbol];
}

// Look up the successor state for a nonterminal `(state, symbol)` (`token_count <= symbol < symbol_count`).
static inline TSStateId ts_language_lookup_goto(
  const TSLanguage *self,
  TSStateId state,
  TSSymbol symbol
) {
  uint32_t nonterm_count = self->symbol_count - self->token_count;
  const uint16_t *dir = &self->small_parse_table[nonterm_count];
  uint32_t block = state / 64u;
  const uint16_t *blk = &dir[block * 6u];
  uint64_t bits = (uint64_t)blk[0] | ((uint64_t)blk[1] << 16) |
    ((uint64_t)blk[2] << 32) | ((uint64_t)blk[3] << 48);
  uint64_t mask = (uint64_t)1 << (state % 64u);
  if (!(bits & mask)) return 0;
  uint32_t rank = ts_language__small_offset(blk + 4) +
    ts_language__count_ones64(bits & (mask - 1u));
  uint32_t dir_blocks = (self->state_count + 63u) / 64u;
  const uint16_t *goto_map = &dir[dir_blocks * 6u + rank * 2u];
  uint16_t value = ts_language_lookup_small_row(self, ts_language__small_offset(goto_map), symbol);
  if (value == UINT16_MAX && self->state_count <= UINT16_MAX) return state;
  return value;
}

static inline void ts_lookahead_iterator__start_small_row(LookaheadIterator *self) {
  const TSLanguage *language = self->language;
  uint32_t index = language->small_parse_table_map[self->state - language->large_state_count];
  uint32_t encoding = index & SMALL_STATE_ENCODING_MASK;
  uint32_t offset = index & SMALL_STATE_OFFSET_MASK;
  if (encoding == SMALL_STATE_DESCRIPTOR_FLAG) {
    const uint16_t *descriptor = &language->small_parse_table[offset];
    const uint16_t *set = &language->small_parse_table[ts_language__small_offset(descriptor)];
    self->descriptor = descriptor;
    self->set_symbol = set[0] * 16u - 16u;
    self->set_data = set + 2;
    self->group_end = self->set_data + set[1];
    self->set_bits = 0;
    return;
  }
  if (encoding == SMALL_STATE_BITMAP_FLAG) {
    const uint16_t *row = &language->small_parse_table[offset];
    const uint16_t *hdr = &language->small_parse_table[ts_language__small_offset(row)];
    self->is_bitmap_state = true;
    self->data = row + 2;
    self->set_symbol = hdr[0] * 64u - 64u;
    self->set_data = hdr + 2;
    self->group_end = self->set_data + hdr[1] * 5u;
    return;
  }
  self->is_pair_state = encoding == SMALL_STATE_PAIR_FLAG;
  self->is_set_state = encoding == SMALL_STATE_SET_FLAG;
  self->data = &language->small_parse_table[offset];
  self->group_end = self->is_set_state ? NULL : self->data + 1;
  self->group_count = *self->data;
  if (self->is_pair_state || self->is_set_state) self->data++;
}

// Iterate over all terminal symbols that have actions in `state`.
static inline LookaheadIterator ts_language_lookaheads(
  const TSLanguage *self,
  TSStateId state
) {
  LookaheadIterator result = {
    .language = self,
    .is_small_state = state >= self->large_state_count,
    .state = state,
    .phase = LookaheadFresh,
    .symbol = UINT16_MAX,
  };
  if (result.is_small_state) {
    ts_lookahead_iterator__start_small_row(&result);
  } else if (self->token_count > 0) {
    result.data = &self->parse_table[(uint32_t)state * self->token_count];
  }
  return result;
}

static inline bool ts_lookahead_iterator__advance(LookaheadIterator *self);
static inline bool ts_lookahead_iterator__next_described(LookaheadIterator *self);
static inline bool ts_lookahead_iterator__next_set(LookaheadIterator *self);
static inline bool ts_lookahead_iterator__next_bitmap(LookaheadIterator *self);
static inline bool ts_lookahead_iterator__next_group(LookaheadIterator *self);

static inline bool ts_lookahead_iterator__next(LookaheadIterator *self) {
  if (self->phase == LookaheadDone) return false;
  if (!ts_lookahead_iterator__advance(self)) {
    self->phase = LookaheadDone;
    return false;
  }
  self->actions = &self->language->parse_actions[self->table_value & TS_PARSE_TABLE_ACTION_MASK];
  self->action_count = *self->actions >> TS_PARSE_ACTION_COUNT_SHIFT;
  self->phase = LookaheadPositioned;
  return true;
}

static inline bool ts_lookahead_iterator__advance(LookaheadIterator *self) {
  if (self->descriptor) return ts_lookahead_iterator__next_described(self);
  if (self->is_set_state) return ts_lookahead_iterator__next_set(self);
  if (self->is_bitmap_state) return ts_lookahead_iterator__next_bitmap(self);
  if (self->is_pair_state) {
    if (!self->group_count) return false;
    self->group_count--;
    self->symbol = *(self->data++);
    self->table_value = *(self->data++);
    return true;
  }
  if (self->is_small_state) {
    if (self->phase == LookaheadPositioned && self->data + 1 < self->group_end) {
      self->symbol = *(++self->data);
      return true;
    }
    return ts_lookahead_iterator__next_group(self);
  }
  uint32_t symbol = self->phase == LookaheadFresh ? 0 : self->symbol + 1u;
  while (symbol < self->language->token_count && !self->data[symbol]) symbol++;
  if (symbol >= self->language->token_count) return false;
  self->symbol = (TSSymbol)symbol;
  self->table_value = self->data[symbol];
  return true;
}

static inline bool ts_lookahead_iterator__next_described(LookaheadIterator *self) {
  while (!self->set_bits) {
    if (self->set_data == self->group_end) return false;
    self->set_bits = *(self->set_data++);
    self->set_symbol += 16;
  }
  self->symbol = self->set_symbol + ts_language__trailing_zeros(self->set_bits);
  self->set_bits &= self->set_bits - 1;
  self->table_value = ts_language_lookup_described(self->language, self->descriptor, self->symbol);
  return true;
}

static inline bool ts_lookahead_iterator__next_set(LookaheadIterator *self) {
  while (!self->set_bits) {
    if (self->set_data != self->group_end) {
      self->set_bits = *(self->set_data++);
      self->set_symbol += 16;
    } else if (self->group_count) {
      self->group_count--;
      self->table_value = *(self->data++);
      uint32_t offset = self->data[0] | ((uint32_t)self->data[1] << 16);
      self->data += 2;
      const uint16_t *set = &self->language->small_parse_table[offset];
      self->set_symbol = set[0] * 16u - 16;
      self->set_data = set + 2;
      self->group_end = self->set_data + set[1];
    } else {
      return false;
    }
  }
  self->symbol = self->set_symbol + ts_language__trailing_zeros(self->set_bits);
  self->set_bits &= self->set_bits - 1;
  return true;
}

static inline bool ts_lookahead_iterator__next_bitmap(LookaheadIterator *self) {
  while (!self->bitmap_bits) {
    if (self->set_data == self->group_end) return false;
    const uint16_t *blk = self->set_data;
    self->set_data += 5;
    self->set_symbol += 64;
    self->bitmap_bits = (uint64_t)blk[0] | ((uint64_t)blk[1] << 16) |
      ((uint64_t)blk[2] << 32) | ((uint64_t)blk[3] << 48);
  }
  self->symbol = (TSSymbol)(self->set_symbol + ts_language__trailing_zeros64(self->bitmap_bits));
  self->bitmap_bits &= self->bitmap_bits - 1u;
  self->table_value = *(self->data++);
  return true;
}

static inline bool ts_lookahead_iterator__next_group(LookaheadIterator *self) {
  if (self->group_count == 0) return false;
  self->data++;
  self->group_count--;
  self->table_value = *(self->data++);
  unsigned symbol_count = *(self->data++);
  self->group_end = self->data + symbol_count;
  self->symbol = *self->data;
  return true;
}

static inline uint32_t ts_language_external_state_stride(const TSLanguage *self) {
  return self->external_state_stride ? self->external_state_stride : self->external_token_count;
}

static inline const bool *ts_language_enabled_external_tokens(
  const TSLanguage *self,
  unsigned external_scanner_state
) {
  if (external_scanner_state == 0) {
    return NULL;
  } else {
    return self->external_scanner.states + ts_language_external_state_stride(self) * external_scanner_state;
  }
}

static inline uint32_t ts_language_alias_sequence_offset(const TSLanguage *self, uint32_t production_id) {
  if (self->alias_sequence_offsets) {
    return self->alias_sequence_offsets[production_id];
  }
  return production_id * self->max_alias_sequence_length;
}

static inline const TSSymbol *ts_language_alias_sequence(const TSLanguage *self, uint32_t production_id) {
  uint32_t offset = ts_language_alias_sequence_offset(self, production_id);
  return offset ? self->alias_sequences + offset : NULL;
}

static inline TSSymbol ts_language_alias_at(const TSLanguage *self, uint32_t production_id, uint32_t child_index) {
  uint32_t offset = ts_language_alias_sequence_offset(self, production_id);
  return offset ? self->alias_sequences[offset + child_index] : 0;
}

static inline void ts_language_field_map(
  const TSLanguage *self,
  uint32_t production_id,
  const TSFieldMapEntry **start,
  const TSFieldMapEntry **end
) {
  if (self->field_count == 0) {
    *start = NULL;
    *end = NULL;
    return;
  }

  TSMapSlice slice = self->field_map_slices[production_id];
  *start = &self->field_map_entries[slice.index];
  *end = &self->field_map_entries[slice.index] + slice.length;
}

static inline void ts_language_write_symbol_as_dot_string(
  const TSLanguage *self,
  FILE *f,
  TSSymbol symbol
) {
  const char *name = ts_language_symbol_name(self, symbol);
  for (const char *chr = name; *chr; chr++) {
    switch (*chr) {
      case '"':
      case '\\':
        fputc('\\', f);
        fputc(*chr, f);
        break;
      case '\n':
        fputs("\\n", f);
        break;
      case '\t':
        fputs("\\t", f);
        break;
      default:
        fputc(*chr, f);
        break;
    }
  }
}

static inline uint32_t ts_language__trailing_zeros(uint32_t bits) {
#if defined(__clang__) || defined(__GNUC__)
  return __builtin_ctz(bits);
#else
  uint32_t count = 0;
  while (!(bits & 1)) {
    bits >>= 1;
    count++;
  }
  return count;
#endif
}

static inline uint32_t ts_language__count_ones64(uint64_t bits) {
#if defined(__clang__) || defined(__GNUC__)
  return (uint32_t)__builtin_popcountll(bits);
#else
  bits -= (bits >> 1) & UINT64_C(0x5555555555555555);
  bits = (bits & UINT64_C(0x3333333333333333)) + ((bits >> 2) & UINT64_C(0x3333333333333333));
  bits = (bits + (bits >> 4)) & UINT64_C(0x0f0f0f0f0f0f0f0f);
  return (uint32_t)((bits * UINT64_C(0x0101010101010101)) >> 56);
#endif
}

static inline uint32_t ts_language__trailing_zeros64(uint64_t bits) {
#if defined(__clang__) || defined(__GNUC__)
  return (uint32_t)__builtin_ctzll(bits);
#else
  uint32_t count = 0;
  while (!(bits & 1u)) {
    bits >>= 1;
    count++;
  }
  return count;
#endif
}

#ifdef __cplusplus
}
#endif

#endif  // TREE_SITTER_LANGUAGE_H_
