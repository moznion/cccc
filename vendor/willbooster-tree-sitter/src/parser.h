#ifndef TREE_SITTER_PARSER_H_
#define TREE_SITTER_PARSER_H_

#ifdef __cplusplus
extern "C" {
#endif

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

#define ts_builtin_sym_error ((TSSymbol)-1)
#define ts_builtin_sym_end 0
#define TREE_SITTER_SERIALIZATION_BUFFER_SIZE 1024

#ifndef TREE_SITTER_API_H_
typedef uint16_t TSStateId;
typedef uint16_t TSSymbol;
typedef uint16_t TSFieldId;
typedef struct TSLanguage TSLanguage;
typedef struct TSLanguageMetadata {
  uint8_t major_version;
  uint8_t minor_version;
  uint8_t patch_version;
} TSLanguageMetadata;
#endif

typedef struct {
  TSFieldId field_id;
  uint8_t child_index;
  bool inherited;
} TSFieldMapEntry;

// Used to index the field and supertype maps.
typedef struct {
  uint16_t index;
  uint16_t length;
} TSMapSlice;

typedef struct {
  bool visible;
  bool named;
  bool supertype;
  bool immediate;
} TSSymbolMetadata;

#define TS_SYMBOL_METADATA_VISIBLE 0x01u
#define TS_SYMBOL_METADATA_NAMED 0x02u
#define TS_SYMBOL_METADATA_SUPERTYPE 0x04u
#define TS_SYMBOL_METADATA_IMMEDIATE 0x08u

#define TS_LANGUAGE_FLAG_WASM 0x01u
#define TS_LANGUAGE_FLAG_UNPARSEABLE 0x02u

typedef struct TSLexer TSLexer;

typedef struct {
  uint32_t bytes;
  uint32_t row;
  uint32_t column;
} TSLexerPosition;

/**
 * Checkpoints may only restore marks during the lex invocation that captured them.
 * Their contents must not be inspected or serialized as scanner state.
 */
typedef struct {
  uint32_t _opaque[3];
} TSLexerCheckpoint;

struct TSLexer {
  int32_t lookahead;
  TSSymbol result_symbol;
  void (*advance)(TSLexer *, bool);
  void (*mark_end)(TSLexer *);
  uint32_t (*get_column)(TSLexer *);
  bool (*is_at_included_range_start)(const TSLexer *);
  bool (*eof)(const TSLexer *);
  void (*log)(const TSLexer *, const char *, ...);
  void (*get_position)(const TSLexer *, TSLexerPosition *);
  void (*mark_start)(TSLexer *, const TSLexerCheckpoint *);
  void (*mark_end_at)(TSLexer *, const TSLexerCheckpoint *);
  void (*get_mark_end)(const TSLexer *, TSLexerPosition *);
  uint32_t (*get_display_column)(TSLexer *, uint32_t tab_width);
  void (*get_checkpoint)(const TSLexer *, TSLexerCheckpoint *);
  void (*get_mark_end_checkpoint)(const TSLexer *, TSLexerCheckpoint *);
  /**
   * Returns the input from the lookahead on as code units of `*size` bytes (1 for UTF-8, 2 for UTF-16) in the host's
   * byte order, and sets `*count` to the number of them that `advance_units` may consume, which is 0 where the input
   * cannot be read this way. The address may be unaligned, so read a 2-byte unit with `memcpy`. The units and
   * `count` hold until the next call of `advance`, `advance_units`, `get_column` or `get_display_column`, each of
   * which may read the input again; peek again after one of them.
   */
  const void *(*peek_units)(const TSLexer *, uint32_t *count, uint32_t *size);
  /**
   * Consumes the next `count` units that `peek_units` returned, each of which must be an ASCII character.
   * `line_feeds` is the number of `\n` among them and `tail` the number of units after the last `\n`, which is
   * not read when there is none.
   */
  void (*advance_units)(TSLexer *, uint32_t count, uint32_t line_feeds, uint32_t tail);
};

typedef enum {
  TSParseActionTypeShift,
  TSParseActionTypeReduce,
  TSParseActionTypeAccept,
  TSParseActionTypeRecover,
  TSParseActionTypeReduceExtra,
} TSParseActionType;

typedef union {
  struct {
    uint8_t type;
    TSStateId state;
    bool extra;
  } shift;
  struct {
    uint8_t type;
    uint8_t child_count;
    TSSymbol symbol;
    int16_t dynamic_precedence;
    uint16_t production_id;
  } reduce;
  uint8_t type;
} TSParseAction;

typedef struct {
  uint16_t lex_state;
  uint16_t lex_state_after_extra;
  uint16_t external_lex_state;
  uint16_t external_lex_state_after_extra;
  uint16_t reserved_word_set_id;
} TSLexerMode;

// `parse_actions` packs each action list into one word per action. Every word holds an action's type in bits
// 0-2, a shift's `extra` flag in bit 3, and in bits 8-23 a shift's state or the word index of a reduce's record.
// The first word of a list also holds its action count in bits 25-31; an empty list is a single zero word.
// Terminal parse-table cells store the action list index in bits 0-14 and `reusable` in bit 15. A reduce record
// is two words: the symbol and child count, then the dynamic precedence and production id.
#define TS_PARSE_ACTION_TYPE_MASK 0x7u
#define TS_PARSE_ACTION_EXTRA 0x8u
#define TS_PARSE_ACTION_MASK 0xffffffu
#define TS_PARSE_ACTION_COUNT_SHIFT 25
#define TS_PARSE_TABLE_REUSABLE_FLAG 0x8000u
#define TS_PARSE_TABLE_ACTION_MASK 0x7fffu

typedef struct {
  int32_t start;
  int32_t end;
} TSCharacterRange;

struct TSLanguage {
  uint32_t abi_version;
  uint32_t symbol_count;
  uint32_t alias_count;
  uint32_t token_count;
  uint32_t external_token_count;
  uint32_t state_count;
  uint32_t large_state_count;
  uint32_t production_id_count;
  uint32_t field_count;
  uint16_t max_alias_sequence_length;
  uint16_t max_reserved_word_set_size;
  const uint16_t *parse_table;
  const uint16_t *small_parse_table;
  const uint32_t *small_parse_table_map;
  const uint32_t *parse_actions;
  const char *string_data;
  uint32_t string_data_size;
  const uint32_t *symbol_name_offsets;
  const uint32_t *field_name_offsets;
  const TSMapSlice *field_map_slices;
  const TSFieldMapEntry *field_map_entries;
  const uint8_t *symbol_metadata;
  const TSSymbol *public_symbol_map;
  const TSSymbol *alias_sequences;
  const uint32_t *alias_sequence_offsets;
  uint32_t alias_sequence_count;
  const TSLexerMode *lex_modes;
  const void *lex_mode_ids;
  uint32_t lex_mode_count;
  const uint16_t *lex_table;
  const uint16_t *keyword_lex_table;
  TSSymbol keyword_capture_token;
  uint8_t flags;
  struct {
    const bool *states;
    const TSSymbol *symbol_map;
    void *(*create)(void);
    void (*destroy)(void *);
    bool (*scan)(void *, TSLexer *, const bool *symbol_whitelist);
    unsigned (*serialize)(void *, char *);
    void (*deserialize)(void *, const char *, unsigned);
  } external_scanner;
  uint32_t external_state_stride;
  const char *name;
  const TSSymbol *reserved_words;
  const TSMapSlice *reserved_word_slices;
  uint32_t supertype_count;
  const TSSymbol *supertype_symbols;
  const TSMapSlice *supertype_map_slices;
  const TSSymbol *supertype_map_entries;
  TSLanguageMetadata metadata;
};

static inline bool set_contains(const TSCharacterRange *ranges, uint32_t len, int32_t lookahead) {
  uint32_t index = 0;
  uint32_t size = len - index;
  while (size > 1) {
    uint32_t half_size = size / 2;
    uint32_t mid_index = index + half_size;
    const TSCharacterRange *range = &ranges[mid_index];
    if (lookahead >= range->start && lookahead <= range->end) {
      return true;
    } else if (lookahead > range->end) {
      index = mid_index;
    }
    size -= half_size;
  }
  const TSCharacterRange *range = &ranges[index];
  return (lookahead >= range->start && lookahead <= range->end);
}

#ifdef _MSC_VER
#define UNUSED __pragma(warning(suppress : 4101))
#else
#define UNUSED __attribute__((unused))
#endif

/*
 *  Parse Table Macros
 */

#define SMALL_STATE(id) ((id) - LARGE_STATE_COUNT)

#define STATE(id) id

#define ACTIONS(id) id

#define ACTION_LIST(count) ((uint32_t)(count) << TS_PARSE_ACTION_COUNT_SHIFT)

#define SHIFT(state_value) (TSParseActionTypeShift | ((uint32_t)(state_value) << 8))

#define SHIFT_EXTRA() (TSParseActionTypeShift | TS_PARSE_ACTION_EXTRA)

#define REDUCE(record) (TSParseActionTypeReduce | ((uint32_t)(record) << 8))

#define REDUCE_EXTRA(record) (TSParseActionTypeReduceExtra | ((uint32_t)(record) << 8))

#define RECOVER() TSParseActionTypeRecover

#define ACCEPT_INPUT() TSParseActionTypeAccept

#define REDUCE_RECORD(symbol_name, children, precedence, prod_id)       \
  (uint32_t)(symbol_name) | ((uint32_t)(children) << 16),               \
  (uint32_t)(uint16_t)(precedence) | ((uint32_t)(prod_id) << 16)

#ifdef __cplusplus
}
#endif

#endif  // TREE_SITTER_PARSER_H_
