#include "./language.h"
#include "./atomic.h"
#include "./wasm_store.h"
#include "tree_sitter/api.h"
#include <stddef.h>
#include <string.h>

#ifdef __wasm__

typedef struct {
  TSLanguage language;
  volatile uint32_t ref_count;
} TSUnparseableLanguage;

// Linear memory can be shared by multiple WebAssembly instances, but a
// module-defined global belongs to one instance. Assign each instance an ID
// from a counter in shared memory so trees can identify the function table
// that owns their language callbacks.
static volatile uint32_t ts_language_next_context_id;

__asm__(
  ".globaltype ts_language_context_id, i32\n"
  "ts_language_context_id:\n"
);

static inline TSUnparseableLanguage *ts_language__unparseable(const TSLanguage *self) {
  return (TSUnparseableLanguage *)((char *)self - offsetof(TSUnparseableLanguage, language));
}

static inline bool ts_language__is_unparseable(const TSLanguage *self) {
  return self && ts_language_has_supported_abi(self) &&
    (self->flags & TS_LANGUAGE_FLAG_UNPARSEABLE) != 0;
}

uint32_t ts_language_current_context_id(void) {
  uint32_t result;
  __asm__(
    "global.get ts_language_context_id\n"
    "local.set %0\n"
    : "=r"(result)
  );
  if (!result) {
    result = atomic_inc(&ts_language_next_context_id);
    __asm__(
      "local.get %0\n"
      "global.set ts_language_context_id\n"
      :
      : "r"(result)
    );
  }
  return result;
}

#endif

const TSLanguage *ts_language_copy(const TSLanguage *self) {
#ifdef __wasm__
  if (ts_language__is_unparseable(self)) {
    atomic_inc(&ts_language__unparseable(self)->ref_count);
  } else
#endif
  if (self && ts_language_is_wasm(self)) {
    ts_wasm_language_retain(self);
  }
  return self;
}

void ts_language_delete(const TSLanguage *self) {
#ifdef __wasm__
  if (ts_language__is_unparseable(self)) {
    TSUnparseableLanguage *language = ts_language__unparseable(self);
    if (atomic_dec(&language->ref_count) == 0) {
      ts_free(language);
    }
  } else
#endif
  if (self && ts_language_is_wasm(self)) {
    ts_wasm_language_release(self);
  }
}

bool ts_language_is_parseable(const TSLanguage *self) {
  return self && ts_language_has_supported_abi(self) &&
    self->lex_table != NULL &&
    !(self->flags & TS_LANGUAGE_FLAG_UNPARSEABLE);
}

const TSLanguage *ts_language_copy_without_callbacks(const TSLanguage *self) {
#ifdef __wasm__
  if (self && ts_language_is_parseable(self)) {
    TSUnparseableLanguage *result = ts_calloc(1, sizeof(TSUnparseableLanguage));
    memcpy(&result->language, self, sizeof(TSLanguage));
    result->language.lex_table = NULL;
    result->language.keyword_lex_table = NULL;
    result->language.flags =
      (uint8_t)((self->flags & ~TS_LANGUAGE_FLAG_WASM) | TS_LANGUAGE_FLAG_UNPARSEABLE);
    result->language.external_scanner.states = NULL;
    result->language.external_scanner.create = NULL;
    result->language.external_scanner.destroy = NULL;
    result->language.external_scanner.scan = NULL;
    result->language.external_scanner.serialize = NULL;
    result->language.external_scanner.deserialize = NULL;
    result->ref_count = 1;
    return &result->language;
  }
#endif
  return ts_language_copy(self);
}

uint32_t ts_language_symbol_count(const TSLanguage *self) {
  return self->symbol_count + self->alias_count;
}

uint32_t ts_language_state_count(const TSLanguage *self) {
  return self->state_count;
}

const TSSymbol *ts_language_supertypes(const TSLanguage *self, uint32_t *length) {
  if (!ts_language_has_supported_abi(self)) {
    *length = 0;
    return NULL;
  }
  *length = self->supertype_count;
  return self->supertype_symbols;
}

const TSSymbol *ts_language_subtypes(
  const TSLanguage *self,
  TSSymbol supertype,
  uint32_t *length
) {
  if (!ts_language_has_supported_abi(self) || supertype >= ts_language_symbol_count(self)) {
    *length = 0;
    return NULL;
  }

  supertype = ts_language_public_symbol(self, supertype);
  if (!ts_language_symbol_metadata(self, supertype).supertype) {
    *length = 0;
    return NULL;
  }

  TSMapSlice slice = self->supertype_map_slices[supertype];
  *length = slice.length;
  return &self->supertype_map_entries[slice.index];
}

uint32_t ts_language_abi_version(const TSLanguage *self) {
  return self->abi_version;
}

const TSLanguageMetadata *ts_language_metadata(const TSLanguage *self) {
  return ts_language_has_supported_abi(self) ? &self->metadata : NULL;
}

const char *ts_language_name(const TSLanguage *self) {
  return ts_language_has_supported_abi(self) ? self->name : NULL;
}

uint32_t ts_language_field_count(const TSLanguage *self) {
  return self->field_count;
}

void ts_language_table_entry(
  const TSLanguage *self,
  TSStateId state,
  TSSymbol symbol,
  TableEntry *result
) {
  if (symbol >= self->token_count) {
    result->action_count = 0;
    result->is_reusable = false;
    result->actions = NULL;
    return;
  }
  uint16_t cell = ts_language_lookup(self, state, symbol);
  const uint32_t *actions = &self->parse_actions[cell & TS_PARSE_TABLE_ACTION_MASK];
  result->action_count = *actions >> TS_PARSE_ACTION_COUNT_SHIFT;
  result->is_reusable = (cell & TS_PARSE_TABLE_REUSABLE_FLAG) != 0;
  result->actions = actions;
}

static uint16_t ts_language_lookup_shared_set(
  const TSLanguage *self,
  uint32_t offset,
  TSSymbol symbol
);
static uint16_t ts_language_lookup_grouped(const uint16_t *data, TSSymbol symbol);
static uint16_t ts_language_lookup_bitmap(
  const TSLanguage *self,
  const uint16_t *data,
  TSSymbol symbol
);

uint16_t ts_language_lookup_small_compact(
  const TSLanguage *self,
  uint32_t index,
  TSSymbol symbol
) {
  uint32_t encoding = index & SMALL_STATE_ENCODING_MASK;
  uint32_t offset = index & SMALL_STATE_OFFSET_MASK;
  if (encoding == SMALL_STATE_SET_FLAG) {
    return ts_language_lookup_shared_set(self, offset, symbol);
  }
  if (encoding == SMALL_STATE_BITMAP_FLAG) {
    return ts_language_lookup_bitmap(self, &self->small_parse_table[offset], symbol);
  }
  if (encoding == SMALL_STATE_PAIR_FLAG) {
    const uint16_t *pairs = &self->small_parse_table[offset];
    uint32_t low = 0, high = *(pairs++);
    while (low < high) {
      uint32_t mid = low + (high - low) / 2;
      TSSymbol candidate = pairs[mid * 2];
      if (candidate < symbol) low = mid + 1;
      else if (candidate > symbol) high = mid;
      else return pairs[mid * 2 + 1];
    }
    return 0;
  }
  return ts_language_lookup_grouped(&self->small_parse_table[offset], symbol);
}

static uint16_t ts_language_lookup_bitmap(
  const TSLanguage *self,
  const uint16_t *data,
  TSSymbol symbol
) {
  const uint16_t *hdr = &self->small_parse_table[ts_language__small_offset(data)];
  uint32_t block = symbol / 64u - hdr[0];
  if (block >= hdr[1]) return 0;
  const uint16_t *blk = &hdr[2u + block * 5u];
  uint64_t bits = (uint64_t)blk[0] | ((uint64_t)blk[1] << 16) |
    ((uint64_t)blk[2] << 32) | ((uint64_t)blk[3] << 48);
  uint64_t mask = (uint64_t)1 << (symbol % 64u);
  if (!(bits & mask)) return 0;
  uint32_t rank = blk[4] + ts_language__count_ones64(bits & (mask - 1u));
  return data[2u + rank];
}

static uint16_t ts_language_lookup_grouped(const uint16_t *data, TSSymbol symbol) {
  uint16_t group_count = *(data++);
  for (unsigned i = 0; i < group_count; i++) {
    uint16_t section_value = *(data++);
    uint16_t symbol_count = *(data++);
    if (symbol_count >= 16) {
      uint32_t low = 0, high = symbol_count;
      while (low < high) {
        uint32_t mid = low + (high - low) / 2;
        if (data[mid] < symbol) low = mid + 1;
        else if (data[mid] > symbol) high = mid;
        else return section_value;
      }
      data += symbol_count;
      continue;
    }
    while (symbol_count >= 4) {
      bool matches = data[0] == symbol;
      matches |= data[1] == symbol;
      matches |= data[2] == symbol;
      matches |= data[3] == symbol;
      if (matches) return section_value;
      data += 4;
      symbol_count -= 4;
    }
    for (unsigned j = 0; j < symbol_count; j++) {
      if (*(data++) == symbol) return section_value;
    }
  }
  return 0;
}

static uint16_t ts_language_lookup_shared_set(
  const TSLanguage *self,
  uint32_t offset,
  TSSymbol symbol
) {
  const uint16_t *data = &self->small_parse_table[offset];
  uint32_t count = *(data++);
  uint32_t block = symbol / 16;
  uint32_t bit = 1u << (symbol % 16);
  for (uint32_t i = 0; i < count; i++, data += 3) {
    uint32_t set_offset = data[1] | ((uint32_t)data[2] << 16);
    const uint16_t *set = &self->small_parse_table[set_offset];
    uint32_t word = block - set[0];
    if (word < set[1] && (set[word + 2] & bit)) return data[0];
  }
  return 0;
}

TSLexerMode ts_language_lex_mode_for_state(
  const TSLanguage *self,
  TSStateId state
) {
  if (self->lex_mode_ids) state = ts_language_lex_mode_id(self, state);
  return self->lex_modes[state];
}

bool ts_language_is_reserved_word(
  const TSLanguage *self,
  TSStateId state,
  TSSymbol symbol
) {
  TSLexerMode lex_mode = ts_language_lex_mode_for_state(self, state);
  if (lex_mode.reserved_word_set_id > 0) {
    unsigned start = lex_mode.reserved_word_set_id * self->max_reserved_word_set_size;
    unsigned end = start + self->max_reserved_word_set_size;
    if (self->reserved_word_slices) {
      TSMapSlice slice = self->reserved_word_slices[lex_mode.reserved_word_set_id];
      start = slice.index;
      end = start + slice.length;
    }
    for (unsigned i = start; i < end; i++) {
      if (self->reserved_words[i] == symbol) return true;
      if (self->reserved_words[i] == 0) break;
    }
  }
  return false;
}

TSSymbolMetadata ts_language_symbol_metadata(
  const TSLanguage *self,
  TSSymbol symbol
) {
  if (symbol == ts_builtin_sym_error) {
    return (TSSymbolMetadata){.visible = true, .named = true};
  }
  if (symbol == ts_builtin_sym_error_repeat) {
    return (TSSymbolMetadata){.visible = false, .named = false};
  }
  uint8_t flags = self->symbol_metadata[symbol];
  return (TSSymbolMetadata){
    .visible = (flags & TS_SYMBOL_METADATA_VISIBLE) != 0,
    .named = (flags & TS_SYMBOL_METADATA_NAMED) != 0,
    .supertype = (flags & TS_SYMBOL_METADATA_SUPERTYPE) != 0,
    .immediate = (flags & TS_SYMBOL_METADATA_IMMEDIATE) != 0,
  };
}

TSSymbol ts_language_public_symbol(
  const TSLanguage *self,
  TSSymbol symbol
) {
  if (symbol == ts_builtin_sym_error) return symbol;
  return self->public_symbol_map[symbol];
}

const char *ts_language_symbol_name(
  const TSLanguage *self,
  TSSymbol symbol
) {
  if (symbol == ts_builtin_sym_error) return "ERROR";
  if (symbol == ts_builtin_sym_error_repeat) return "_ERROR";
  if (symbol < ts_language_symbol_count(self)) {
    return self->string_data + self->symbol_name_offsets[symbol];
  }
  return NULL;
}

TSSymbol ts_language_symbol_for_name(
  const TSLanguage *self,
  const char *string,
  uint32_t length,
  bool is_named
) {
  if (is_named && length == 5 && !strncmp(string, "ERROR", length)) return ts_builtin_sym_error;
  uint16_t count = (uint16_t)ts_language_symbol_count(self);
  for (TSSymbol i = 0; i < count; i++) {
    TSSymbolMetadata metadata = ts_language_symbol_metadata(self, i);
    if ((!metadata.visible && !metadata.supertype) || metadata.named != is_named) continue;
    const char *symbol_name = self->string_data + self->symbol_name_offsets[i];
    if (!strncmp(symbol_name, string, length) && !symbol_name[length]) {
      return self->public_symbol_map[i];
    }
  }
  return 0;
}

TSSymbolType ts_language_symbol_type(
  const TSLanguage *self,
  TSSymbol symbol
) {
  TSSymbolMetadata metadata = ts_language_symbol_metadata(self, symbol);
  if (metadata.named && metadata.visible) return TSSymbolTypeRegular;
  if (metadata.visible) return TSSymbolTypeAnonymous;
  if (metadata.supertype) return TSSymbolTypeSupertype;
  return TSSymbolTypeAuxiliary;
}

const char *ts_language_field_name_for_id(
  const TSLanguage *self,
  TSFieldId id
) {
  uint32_t count = ts_language_field_count(self);
  if (id > 0 && id <= count) {
    return self->string_data + self->field_name_offsets[id - 1u];
  }
  return NULL;
}

static int ts_language__compare_field_name(
  const char *candidate,
  const char *name,
  uint32_t name_length
);

TSFieldId ts_language_field_id_for_name(
  const TSLanguage *self,
  const char *name,
  uint32_t name_length
) {
  uint32_t low = 0;
  uint32_t high = ts_language_field_count(self);
  while (low < high) {
    uint32_t mid = low + (high - low) / 2u;
    const char *candidate = self->string_data + self->field_name_offsets[mid];
    int cmp = ts_language__compare_field_name(candidate, name, name_length);
    if (cmp < 0) {
      low = mid + 1u;
    } else if (cmp > 0) {
      high = mid;
    } else {
      return (TSFieldId)(mid + 1u);
    }
  }
  return 0;
}

static int ts_language__compare_field_name(
  const char *candidate,
  const char *name,
  uint32_t name_length
) {
  for (uint32_t i = 0; i < name_length; i++) {
    uint8_t a = (uint8_t)candidate[i];
    uint8_t b = (uint8_t)name[i];
    if (a != b) return a < b ? -1 : 1;
    if (a == 0) return -1;
  }
  return candidate[name_length] == '\0' ? 0 : 1;
}
