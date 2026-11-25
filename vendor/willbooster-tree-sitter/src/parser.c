#include <stdio.h>
#include <limits.h>
#include <stdbool.h>
#include <inttypes.h>
#include "tree_sitter/api.h"
#include "./alloc.h"
#include "./array.h"
#include "./error_costs.h"
#include "./language.h"
#include "./length.h"
#include "./lexer.h"
#include "./lex_table.h"
#include "./reduce_action.h"
#include "./stack.h"
#include "./subtree.h"
#include "./table_cache.h"
#include "./tree.h"
#include "./ts_assert.h"
#include "./wasm_store.h"

#define LOG(...)                                                                            \
  if (self->lexer.logger.log || self->dot_graph_file) {                                     \
    snprintf(self->lexer.debug_buffer, TREE_SITTER_SERIALIZATION_BUFFER_SIZE, __VA_ARGS__); \
    ts_parser__log(self);                                                                   \
  }

#define LOG_LOOKAHEAD(symbol_name, size)                      \
  if (self->lexer.logger.log || self->dot_graph_file) {       \
    char *buf = self->lexer.debug_buffer;                     \
    const char *symbol = symbol_name;                         \
    int off = snprintf(                                       \
      buf,                                                    \
      TREE_SITTER_SERIALIZATION_BUFFER_SIZE,                  \
      "lexed_lookahead sym:"                                  \
    );                                                        \
    for (                                                     \
      int i = 0;                                              \
      symbol[i] != '\0'                                       \
      && off < TREE_SITTER_SERIALIZATION_BUFFER_SIZE;         \
      i++                                                     \
    ) {                                                       \
      switch (symbol[i]) {                                    \
      case '\t': buf[off++] = '\\'; buf[off++] = 't'; break;  \
      case '\n': buf[off++] = '\\'; buf[off++] = 'n'; break;  \
      case '\v': buf[off++] = '\\'; buf[off++] = 'v'; break;  \
      case '\f': buf[off++] = '\\'; buf[off++] = 'f'; break;  \
      case '\r': buf[off++] = '\\'; buf[off++] = 'r'; break;  \
      case '\\': buf[off++] = '\\'; buf[off++] = '\\'; break; \
      default:   buf[off++] = symbol[i]; break;               \
      }                                                       \
    }                                                         \
    snprintf(                                                 \
      buf + off,                                              \
      TREE_SITTER_SERIALIZATION_BUFFER_SIZE - off,            \
      ", size:%u",                                            \
      size                                                    \
    );                                                        \
    ts_parser__log(self);                                     \
  }

#ifdef __wasm__
#define LOG_STACK() ((void)0)
#define LOG_TREE(tree) ((void)(tree))
#else
#define LOG_STACK()                                                              \
  if (self->dot_graph_file) {                                                    \
    ts_stack_print_dot_graph(self->stack, self->language, self->dot_graph_file); \
    fputs("\n\n", self->dot_graph_file);                                         \
  }

#define LOG_TREE(tree)                                                      \
  if (self->dot_graph_file) {                                               \
    ts_subtree_print_dot_graph(tree, self->language, self->dot_graph_file); \
    fputs("\n", self->dot_graph_file);                                      \
  }
#endif

#define SYM_NAME(symbol) ts_language_symbol_name(self->language, symbol)

#define TREE_NAME(tree) SYM_NAME(ts_subtree_symbol(tree))

#define MAX_VERSION_COUNT_OVERFLOW 4
#ifndef TREE_SITTER_MAX_VERSION_COUNT
#define TREE_SITTER_MAX_VERSION_COUNT 6
#endif
_Static_assert(
  (TREE_SITTER_MAX_VERSION_COUNT) > 0 &&
  (TREE_SITTER_MAX_VERSION_COUNT) <= UINT_MAX - MAX_VERSION_COUNT_OVERFLOW,
  "TREE_SITTER_MAX_VERSION_COUNT must be positive and leave room for overflow versions"
);
static const unsigned MAX_VERSION_COUNT = (TREE_SITTER_MAX_VERSION_COUNT) | 0;
static const unsigned MAX_SUMMARY_DEPTH = 16;
// Bounds how many reductions error recovery performs in a row on one stack version without reaching a shift, in case a
// parse table's reductions never reach one. A chain that does reach one grows with the nesting depth of the input (about
// four reductions per level of nested Kotlin if-expressions), so the bound must stay far above realistic nesting.
static const unsigned MAX_CHAINED_REDUCTION_COUNT = 4096;
static const unsigned MAX_COST_DIFFERENCE = 18 * ERROR_COST_PER_SKIPPED_TREE;
// A recovery stays provisional, which protects it and its competitors from pruning, until an error-containing reduction
// reaches the root. Inside a construct spanning the rest of the file (a C++ namespace, a C include guard), that happens
// only at EOF, so every competing recovery path stays alive and the parse takes about twice as long. A recovery whose
// version has pushed this many more nodes counts as confirmed; on the C and C++ example corpora, smaller bounds leave
// more C files with additional ERROR nodes.
static const unsigned MAX_PROVISIONAL_RECOVERY_NODE_COUNT = 300;
static const unsigned OP_COUNT_PER_PARSER_CALLBACK_CHECK = 100;

typedef struct {
  uint64_t hash;
  uint32_t offset;
  uint32_t length;
  uint32_t left;
  uint32_t right;
  uint32_t level;
} ScannerStateEntry;

typedef struct {
  Array(char) bytes;
  Array(ScannerStateEntry) entries;
  Array(uint32_t) buckets;
  uint64_t seed;
} ScannerStateTable;

typedef struct {
  LookaheadToken token;
  uint32_t scanner_state_id;
  uint32_t byte_index;
  bool after_extra;
} TokenCache;

struct TSParser {
  Lexer lexer;
  Stack *stack;
  SubtreePool tree_pool;
  ScannerStateTable scanner_states;
  const TSLanguage *language;
  TSWasmStore *wasm_store;
  ReduceActionSet reduce_actions;
  Subtree finished_tree;
  SubtreeArray trailing_extras;
  SubtreeArray trailing_extras2;
  SubtreeArray scratch_trees;
  TokenCache token_cache;
  void *external_scanner_payload;
  FILE *dot_graph_file;
  unsigned accept_count;
  uint64_t operation_count;
  uint64_t total_operation_count;
  uint64_t max_operations;
  StackVersion accept_version;
  StackSliceArray accept_slices;
  uint32_t accept_index;
  bool accept_collected;
  bool accept_from_condense;
  unsigned accept_min_error_cost;
  TSParseOptions parse_options;
  TSParseState parse_state;
  bool has_scanner_error;
  bool canceled_balancing;
  bool exceeded_max_operations;
  bool has_error;
  // Allocated by the first parse, so that a parser that never parses costs no cache;
  // `ts_parser_swap_table_cache` lends it.
  TableCache *table_cache;
};

typedef struct {
  unsigned cost;
  unsigned node_count;
  int dynamic_precedence;
  bool is_in_error;
  bool is_paused;
  bool is_unconfirmed;
  uint64_t unconfirmed_recovery;
} ErrorStatus;

typedef enum {
  ErrorComparisonTakeLeft,
  ErrorComparisonPreferLeft,
  ErrorComparisonNone,
  ErrorComparisonPreferRight,
  ErrorComparisonTakeRight,
} ErrorComparison;

typedef struct {
  const char *string;
  uint32_t length;
} TSStringInput;

// StringInput

static const char *ts_string_input_read(
  void *_self,
  uint32_t byte,
  TSPoint point,
  uint32_t *length
) {
  (void)point;
  TSStringInput *self = (TSStringInput *)_self;
  if (byte >= self->length) {
    *length = 0;
    return "";
  } else {
    *length = self->length - byte;
    return self->string + byte;
  }
}

// Parser - Private

static void ts_parser__log(TSParser *self) {
  if (self->lexer.logger.log) {
    self->lexer.logger.log(
      self->lexer.logger.payload,
      TSLogTypeParse,
      self->lexer.debug_buffer
    );
  }

  if (self->dot_graph_file) {
    fprintf(self->dot_graph_file, "graph {\nlabel=\"");
    for (char *chr = &self->lexer.debug_buffer[0]; *chr != 0; chr++) {
      if (*chr == '"' || *chr == '\\') fputc('\\', self->dot_graph_file);
      fputc(*chr, self->dot_graph_file);
    }
    fprintf(self->dot_graph_file, "\"\n}\n\n");
  }
}

static void ts_parser__log_version_limit(
  TSParser *self,
  const char *operation,
  StackVersion version,
  bool limit_reached
) {
  if (!limit_reached) return;
  LOG("version_limit operation:%s, version:%u, limit:%u, overflow:%u, halted:%u",
    operation, version, MAX_VERSION_COUNT, MAX_VERSION_COUNT_OVERFLOW,
    ts_stack_halted_version_count(self->stack));
}

static ErrorComparison ts_parser__compare_versions(
  TSParser *self,
  ErrorStatus a,
  ErrorStatus b
) {
  (void)self;
  if (!a.is_in_error && b.is_in_error) {
    if (a.cost < b.cost) {
      return ErrorComparisonTakeLeft;
    } else {
      return ErrorComparisonPreferLeft;
    }
  }

  if (a.is_in_error && !b.is_in_error) {
    if (b.cost < a.cost) {
      return ErrorComparisonTakeRight;
    } else {
      return ErrorComparisonPreferRight;
    }
  }

  if (a.cost < b.cost) {
    if ((b.cost - a.cost) * (1 + a.node_count) > MAX_COST_DIFFERENCE) {
      return ErrorComparisonTakeLeft;
    } else {
      return ErrorComparisonPreferLeft;
    }
  }

  if (b.cost < a.cost) {
    if ((a.cost - b.cost) * (1 + b.node_count) > MAX_COST_DIFFERENCE) {
      return ErrorComparisonTakeRight;
    } else {
      return ErrorComparisonPreferRight;
    }
  }

  if (a.dynamic_precedence > b.dynamic_precedence) return ErrorComparisonPreferLeft;
  if (b.dynamic_precedence > a.dynamic_precedence) return ErrorComparisonPreferRight;
  return ErrorComparisonNone;
}

// A recovery can resume inside a construct the input never closes. Until it reaches the root
// state or outlives MAX_PROVISIONAL_RECOVERY_NODE_COUNT, its lower error cost must not discard
// another resumed parse.
// Paused heads have not tried recovery for their lookahead yet; an active head
// alone is not evidence that their continuation cannot finish the construct.
static ErrorComparison ts_parser__compare_ranked_versions(
  TSParser *self,
  ErrorStatus higher,
  ErrorStatus lower
) {
  ErrorComparison comparison = ts_parser__compare_versions(self, higher, lower);
  bool displaces_higher = comparison == ErrorComparisonPreferRight || comparison == ErrorComparisonTakeRight;
  bool paused_recovery = higher.is_paused && higher.unconfirmed_recovery;
  bool has_unconfirmed_recovery = lower.unconfirmed_recovery &&
    lower.unconfirmed_recovery != higher.unconfirmed_recovery;
  if (comparison == ErrorComparisonTakeLeft && higher.unconfirmed_recovery &&
      higher.unconfirmed_recovery != lower.unconfirmed_recovery &&
      (!lower.is_in_error || lower.is_paused)) {
    return ErrorComparisonPreferLeft;
  }
  if (displaces_higher &&
      (lower.is_unconfirmed || has_unconfirmed_recovery || paused_recovery) &&
      (!higher.is_in_error || paused_recovery || (has_unconfirmed_recovery && higher.is_paused))) {
    return lower.is_unconfirmed ? ErrorComparisonNone : ErrorComparisonPreferRight;
  }
  return comparison;
}

static ErrorStatus ts_parser__version_status(
  TSParser *self,
  StackVersion version
) {
  unsigned cost = ts_stack_error_cost(self->stack, version);
  bool is_paused = ts_stack_is_paused(self->stack, version);
  if (is_paused) cost += ERROR_COST_PER_SKIPPED_TREE;
  return (ErrorStatus) {
    .cost = cost,
    .node_count = ts_stack_node_count_since_error(self->stack, version),
    .dynamic_precedence = ts_stack_dynamic_precedence(self->stack, version),
    .is_in_error = is_paused || ts_stack_state(self->stack, version) == ERROR_STATE,
    .is_paused = is_paused,
    .is_unconfirmed = ts_stack_has_unconfirmed_missing_token(self->stack, version),
    .unconfirmed_recovery = ts_stack_unconfirmed_recovery(self->stack, version),
  };
}

static bool ts_parser__better_version_exists(
  TSParser *self,
  StackVersion version,
  bool is_in_error,
  unsigned cost
) {
  if (self->finished_tree.ptr && ts_subtree_error_cost(self->finished_tree) <= cost) {
    return true;
  }

  Length position = ts_stack_position(self->stack, version);
  ErrorStatus status = {
    .cost = cost,
    .is_in_error = is_in_error,
    .dynamic_precedence = ts_stack_dynamic_precedence(self->stack, version),
    .node_count = ts_stack_node_count_since_error(self->stack, version),
  };

  for (StackVersion i = 0, n = ts_stack_version_count(self->stack); i < n; i++) {
    if (i == version ||
        !ts_stack_is_active(self->stack, i) ||
        ts_stack_position(self->stack, i).bytes < position.bytes) continue;
    ErrorStatus status_i = ts_parser__version_status(self, i);
    if (status_i.is_unconfirmed || status_i.unconfirmed_recovery) continue;
    switch (ts_parser__compare_versions(self, status, status_i)) {
      case ErrorComparisonTakeRight:
        return true;
      case ErrorComparisonPreferRight:
        if (ts_stack_can_merge(self->stack, i, version)) return true;
        break;
      default:
        break;
    }
  }

  return false;
}

static void ts_parser__external_scanner_create(
  TSParser *self
) {
  if (self->language && self->language->external_scanner.states) {
    if (ts_language_is_wasm(self->language)) {
      self->external_scanner_payload = (void *)(uintptr_t)ts_wasm_store_call_scanner_create(
        self->wasm_store
      );
      if (ts_wasm_store_has_error(self->wasm_store)) {
        self->has_scanner_error = true;
      }
    } else if (self->language->external_scanner.create) {
      self->external_scanner_payload = self->language->external_scanner.create();
    }
  }
}

static void ts_parser__external_scanner_destroy(
  TSParser *self
) {
  if (
    self->language &&
    self->external_scanner_payload &&
    self->language->external_scanner.destroy &&
    !ts_language_is_wasm(self->language)
  ) {
    self->language->external_scanner.destroy(
      self->external_scanner_payload
    );
  }
  self->external_scanner_payload = NULL;
}

static unsigned ts_parser__external_scanner_serialize(TSParser *self) {
  uint32_t length = ts_language_is_wasm(self->language)
    ? ts_wasm_store_call_scanner_serialize(
        self->wasm_store, (uintptr_t)self->external_scanner_payload, self->lexer.debug_buffer
      )
    : self->language->external_scanner.serialize(
        self->external_scanner_payload, self->lexer.debug_buffer
      );
  if (ts_language_is_wasm(self->language) && ts_wasm_store_has_error(self->wasm_store)) {
    self->has_scanner_error = true;
  }
  ts_assert(length <= TREE_SITTER_SERIALIZATION_BUFFER_SIZE);
  return length;
}

static const char *scanner_state_table_get(
  const ScannerStateTable *self,
  uint32_t id,
  uint32_t *length
);
static uint32_t scanner_state_table_intern(
  ScannerStateTable *self,
  const char *data,
  uint32_t length
);

static void ts_parser__external_scanner_deserialize(
  TSParser *self,
  uint32_t scanner_state_id
) {
  uint32_t length = 0;
  const char *data = scanner_state_table_get(&self->scanner_states, scanner_state_id, &length);
  if (ts_language_is_wasm(self->language)) {
    ts_wasm_store_call_scanner_deserialize(
      self->wasm_store, (uintptr_t)self->external_scanner_payload, data, length
    );
    if (ts_wasm_store_has_error(self->wasm_store)) self->has_scanner_error = true;
  } else {
    self->language->external_scanner.deserialize(self->external_scanner_payload, data, length);
  }
}

static const char *scanner_state_table_get(
  const ScannerStateTable *self,
  uint32_t id,
  uint32_t *length
) {
  if (id == 0) {
    *length = 0;
    return NULL;
  }
  ScannerStateEntry entry = self->entries.contents[id - 1];
  *length = entry.length;
  return self->bytes.contents + entry.offset;
}

static bool ts_parser__external_scanner_scan(
  TSParser *self,
  TSStateId external_lex_state
) {
  if (ts_language_is_wasm(self->language)) {
    bool result = ts_wasm_store_call_scanner_scan(
      self->wasm_store,
      (uintptr_t)self->external_scanner_payload,
      external_lex_state * ts_language_external_state_stride(self->language)
    );
    if (ts_wasm_store_has_error(self->wasm_store)) {
      self->has_scanner_error = true;
    }
    return result;
  } else {
    const bool *valid_external_tokens = ts_language_enabled_external_tokens(
      self->language,
      external_lex_state
    );
    return self->language->external_scanner.scan(
      self->external_scanner_payload,
      &self->lexer.data,
      valid_external_tokens
    );
  }
}

static inline TSLexerMode ts_parser__lex_mode_for_extra_context(TSLexerMode mode, bool after_extra);

static bool ts_parser__can_reuse_cached_token(
  TSParser *self,
  StackVersion version,
  TSStateId state,
  LookaheadToken cached,
  bool leaf_after_extra,
  TableEntry *table_entry
) {
  TSSymbol leaf_symbol = ts_subtree_symbol(cached.subtree);
  bool is_immediate = ts_language_symbol_metadata(self->language, leaf_symbol).immediate;
  bool after_extra = ts_stack_after_extra(self->stack, version);
  if (after_extra != leaf_after_extra) return false;
  if (after_extra && is_immediate) {
    return false;
  }
  TSStateId leaf_state = cached.parse_state;
  if (
    leaf_state != state &&
    (leaf_symbol == self->language->keyword_capture_token || !table_entry->is_reusable) &&
    cached.is_keyword_exact
  ) return false;

  TSLexerMode current_lex_mode = ts_parser__lex_mode_for_extra_context(
    ts_language_lex_mode_for_state(self->language, state), after_extra
  );
  TSLexerMode leaf_lex_mode = ts_parser__lex_mode_for_extra_context(
    ts_language_lex_mode_for_state(self->language, leaf_state), leaf_after_extra
  );

  // At the end of a non-terminal extra, the lexer returns NULL_LOOKAHEAD so the parser
  // reduces at symbol 0 rather than consuming the next token.
  if (current_lex_mode.lex_state == (uint16_t)(-1)) return false;

  if (
    table_entry->action_count > 0 &&
    memcmp(&leaf_lex_mode, &current_lex_mode, sizeof(TSLexerMode)) == 0 &&
    (
      leaf_symbol != self->language->keyword_capture_token ||
      (!cached.is_keyword && leaf_state == state)
    )
  ) return true;

  if (ts_subtree_size(cached.subtree).bytes == 0 && leaf_symbol != ts_builtin_sym_end) return false;

  return current_lex_mode.external_lex_state == 0 && table_entry->is_reusable;
}

typedef struct {
  TSLexerMode lex_mode;
  Length start_position;
  uint32_t external_scanner_state_id;
  uint32_t new_scanner_state_id;
  bool found_external_token;
  bool external_scanner_state_changed;
  bool error_mode;
  bool skipped_error;
  bool retry_internal;
  bool internal_eof;
  bool internal_already_failed;
  Length failed_internal_position;
  Length failed_internal_token_start;
  ColumnData failed_internal_column_data;
  int32_t first_error_character;
  Length error_start_position;
  Length error_end_position;
} LexLoopState;

static bool ts_parser__eof_token_is_usable(TSParser *, StackVersion, TSSymbol);
static inline LexLoopState ts_parser__init_lex_loop(
  TSParser *, StackVersion, TSStateId, TSLexerMode
);
static inline bool ts_parser__lookup_keyword(TSParser *, bool *, TSSymbol *);
static inline int ts_parser__try_lex_external(TSParser *, StackVersion, TSStateId, LexLoopState *);
static inline bool ts_parser__reuse_ext_scan_cache(TSParser *, uint32_t, LexLoopState *, bool *);
static inline int ts_parser__run_ext_scan(TSParser *, Length, LexLoopState *, bool *);
static inline bool ts_parser__accept_empty_ext_token(
  TSParser *, StackVersion, TSStateId, const LexLoopState *
);
static inline bool ts_parser__can_cache_ext_scan(const TSParser *);
static inline bool ts_parser__lex_internal_attempt(TSParser *, LexLoopState *);
static inline bool ts_parser__can_accept_internal_token(TSParser *, TSStateId, bool, bool);
static inline void ts_parser__enter_lex_error_mode(TSParser *, StackVersion, bool, LexLoopState *);
static inline void ts_parser__defer_failed_internal(TSParser *, LexLoopState *);
static inline bool ts_parser__advance_skipped_error(TSParser *, LexLoopState *);
static inline LookaheadToken ts_parser__finish_lexed_token(
  TSParser *, StackVersion, TSStateId, const LexLoopState *
);
static inline bool ts_parser__resolve_keyword(
  TSParser *, StackVersion, TSStateId, TSSymbol *, bool *, bool *
);
static bool ts_parser__recovery_accepts_keyword(
  TSParser *, StackVersion, TSStateId, TSSymbol
);
static Subtree ts_parser__new_decode_error_token(
  SubtreePool *, TSSymbol, Length, Length, const TSLanguage *
);

static bool ts_parser__check_lexer_progress(void *payload, unsigned operations) {
  TSParser *self = payload;
  self->total_operation_count += operations;
  if (self->max_operations && self->total_operation_count > self->max_operations) {
    self->exceeded_max_operations = true;
    return false;
  }
  return true;
}

// code-gauge-allow functionCognitiveComplexity -- coordinates external scanner, internal lexer, error mode, and skipped-error loop
// code-gauge-allow functionNcss -- coordinates external scanner, internal lexer, error mode, and skipped-error loop
static LookaheadToken ts_parser__lex(
  TSParser *self,
  StackVersion version,
  TSStateId parse_state
) {
  TSLexerMode lex_mode = ts_language_lex_mode_for_state(self->language, parse_state);
  if (lex_mode.lex_state == (uint16_t)-1) {
    LOG("no_lookahead_after_non_terminal_extra");
    return NULL_LOOKAHEAD;
  }

  LexLoopState st = ts_parser__init_lex_loop(self, version, parse_state, lex_mode);
  ts_lexer_reset_fast(&self->lexer, st.start_position);

  for (bool did_iterate = false;; did_iterate = true) {
    if (did_iterate && !ts_parser__check_lexer_progress(self, 1)) return NULL_LOOKAHEAD;
    bool internal_only = st.retry_internal;
    st.retry_internal = false;

    if (st.lex_mode.external_lex_state != 0 && !internal_only) {
      int ext = ts_parser__try_lex_external(self, version, parse_state, &st);
      if (ext < 0) return NULL_LOOKAHEAD;
      if (ext > 0) break;
    }

    bool found_token = ts_parser__lex_internal_attempt(self, &st);
    if (self->exceeded_max_operations) return NULL_LOOKAHEAD;
    if (internal_only && found_token && self->lexer.data.result_symbol == ts_builtin_sym_end) {
      st.internal_eof = true;
      ts_lexer_reset(&self->lexer, st.start_position);
      continue;
    }
    if (ts_parser__can_accept_internal_token(self, parse_state, st.error_mode, found_token)) break;

    if (!st.error_mode) {
      ts_parser__enter_lex_error_mode(self, version, found_token, &st);
      continue;
    }
    if (internal_only) {
      ts_parser__defer_failed_internal(self, &st);
      continue;
    }
    if (ts_parser__advance_skipped_error(self, &st)) break;
    if (self->exceeded_max_operations) return NULL_LOOKAHEAD;
  }

  return ts_parser__finish_lexed_token(self, version, parse_state, &st);
}

static inline LexLoopState ts_parser__init_lex_loop(
  TSParser *self,
  StackVersion version,
  TSStateId parse_state,
  TSLexerMode lex_mode
) {
  LexLoopState st;
  st.lex_mode = ts_parser__lex_mode_for_extra_context(
    lex_mode, ts_stack_after_extra(self->stack, version)
  );
  st.start_position = ts_stack_position(self->stack, version);
  st.external_scanner_state_id = ts_stack_external_scanner_state_id(self->stack, version);
  st.new_scanner_state_id = 0;
  st.found_external_token = false;
  st.external_scanner_state_changed = false;
  st.error_mode = parse_state == ERROR_STATE;
  st.skipped_error = false;
  st.retry_internal = false;
  st.internal_eof = false;
  st.internal_already_failed = false;
  return st;
}

static inline int ts_parser__try_lex_external(
  TSParser *self,
  StackVersion version,
  TSStateId parse_state,
  LexLoopState *st
) {
  Length current_position = self->lexer.current_position;
  ColumnData column_data = self->lexer.column_data;
  bool found_token = false;
  if (!ts_parser__reuse_ext_scan_cache(self, current_position.bytes, st, &found_token)) {
    if (ts_parser__run_ext_scan(self, current_position, st, &found_token) < 0) return -1;
  }
  if (
    found_token &&
    self->lexer.token_end_position.bytes <= current_position.bytes &&
    !ts_parser__accept_empty_ext_token(self, version, parse_state, st)
  ) {
    found_token = false;
  }
  if (found_token) {
    st->found_external_token = true;
    return 1;
  }
  ts_lexer_reset_fast(&self->lexer, current_position);
  self->lexer.column_data = column_data;
  return 0;
}

static inline bool ts_parser__reuse_ext_scan_cache(
  TSParser *self,
  uint32_t start_byte,
  LexLoopState *st,
  bool *found_token
) {
  if (!ts_parser__can_cache_ext_scan(self)) return false;
  for (unsigned i = 0; i < 2u; i++) {
    const TSLexExtScanCache *c = &self->lexer.ext_scan_cache[i];
    if (
      !c->valid ||
      c->start_byte != start_byte ||
      c->external_lex_state != st->lex_mode.external_lex_state ||
      c->scanner_state_id != st->external_scanner_state_id
    ) continue;
    *found_token = c->found_token;
    if (!c->found_token) return true;
    self->lexer.token_start_position = c->token_start;
    self->lexer.token_end_position = c->token_end;
    self->lexer.data.result_symbol = c->result_symbol;
    self->lexer.token_has_decode_error = false;
    st->new_scanner_state_id = c->new_scanner_state_id;
    st->external_scanner_state_changed = c->new_scanner_state_id != st->external_scanner_state_id;
    return true;
  }
  return false;
}

static inline int ts_parser__run_ext_scan(
  TSParser *self,
  Length current_position,
  LexLoopState *st,
  bool *found_token
) {
  LOG(
    "lex_external state:%d, row:%u, column:%u",
    st->lex_mode.external_lex_state,
    current_position.extent.row,
    current_position.extent.column
  );
  bool can_cache = ts_parser__can_cache_ext_scan(self);
  ts_lexer_start(&self->lexer);
  ts_parser__external_scanner_deserialize(self, st->external_scanner_state_id);
  ts_lexer_begin_external_scan(&self->lexer);
  bool found = ts_parser__external_scanner_scan(self, st->lex_mode.external_lex_state);
  ts_lexer_end_external_scan(&self->lexer, current_position, found);
  if (self->has_scanner_error || self->exceeded_max_operations) return -1;
  ts_lexer_finish(&self->lexer);
  if (found) {
    uint32_t state_len = ts_parser__external_scanner_serialize(self);
    if (self->has_scanner_error) return -1;
    st->new_scanner_state_id = scanner_state_table_intern(
      &self->scanner_states, self->lexer.debug_buffer, state_len
    );
    st->external_scanner_state_changed = st->new_scanner_state_id != st->external_scanner_state_id;
  }
  if (can_cache && ts_parser__can_cache_ext_scan(self)) {
    self->lexer.ext_scan_cache[1] = self->lexer.ext_scan_cache[0];
    self->lexer.ext_scan_cache[0] = (TSLexExtScanCache){
      .token_start = self->lexer.token_start_position,
      .token_end = self->lexer.token_end_position,
      .start_byte = current_position.bytes,
      .scanner_state_id = st->external_scanner_state_id,
      .new_scanner_state_id = found ? st->new_scanner_state_id : 0,
      .external_lex_state = st->lex_mode.external_lex_state,
      .result_symbol = self->lexer.data.result_symbol,
      .found_token = found,
      .valid = true,
    };
  }
  *found_token = found;
  return 0;
}

static inline bool ts_parser__accept_empty_ext_token(
  TSParser *self,
  StackVersion version,
  TSStateId parse_state,
  const LexLoopState *st
) {
  TSSymbol symbol = self->language->external_scanner.symbol_map[self->lexer.data.result_symbol];
  bool unusable_eof = st->internal_eof && !ts_parser__eof_token_is_usable(self, version, symbol);
  bool is_extra = ts_table_cache_next_state(self->table_cache, self->language, parse_state, symbol) == parse_state;
  if ((!st->external_scanner_state_changed || unusable_eof) &&
      (st->error_mode || !ts_stack_has_advanced_since_error(self->stack, version) || is_extra)) {
    LOG("ignore_empty_external_token symbol:%s", SYM_NAME(symbol));
    return false;
  }
  return true;
}

static inline bool ts_parser__can_cache_ext_scan(const TSParser *self) {
  return !self->lexer.logger.log &&
    !self->dot_graph_file &&
    !self->lexer.track_columns &&
    !self->lexer.has_operation_limit;
}

static inline bool ts_parser__lex_internal_attempt(TSParser *self, LexLoopState *st) {
  if (st->internal_already_failed) {
    ts_lexer_reset(&self->lexer, st->failed_internal_position);
    ts_lexer_start(&self->lexer);
    self->lexer.token_start_position = st->failed_internal_token_start;
    self->lexer.column_data = st->failed_internal_column_data;
    st->internal_already_failed = false;
    return false;
  }
  LOG(
    "lex_internal state:%d, row:%u, column:%u",
    st->lex_mode.lex_state,
    self->lexer.current_position.extent.row,
    self->lexer.current_position.extent.column
  );
  ts_lexer_start_fast(&self->lexer);
  bool found = ts_lexer_lex_table(&self->lexer, self->language->lex_table, st->lex_mode.lex_state);
  if (!self->exceeded_max_operations) ts_lexer_finish_fast(&self->lexer);
  return found;
}

static inline bool ts_parser__can_accept_internal_token(
  TSParser *self,
  TSStateId parse_state,
  bool error_mode,
  bool found_token
) {
  if (!found_token) return false;
  if (error_mode || self->lexer.data.result_symbol != ts_builtin_sym_end) return true;
  return ts_table_cache_has_actions(self->table_cache, self->language, parse_state, ts_builtin_sym_end);
}

static inline void ts_parser__enter_lex_error_mode(
  TSParser *self,
  StackVersion version,
  bool found_token,
  LexLoopState *st
) {
  st->internal_eof = found_token && self->lexer.data.result_symbol == ts_builtin_sym_end;
  st->error_mode = true;
  st->lex_mode = ts_parser__lex_mode_for_extra_context(
    ts_language_lex_mode_for_state(self->language, ERROR_STATE),
    ts_stack_after_extra(self->stack, version)
  );
  ts_lexer_reset(&self->lexer, st->start_position);
  st->retry_internal = st->lex_mode.external_lex_state != 0 &&
    !self->lexer.data.eof(&self->lexer.data);
  if (!st->retry_internal || self->language->keyword_capture_token == 0) return;
  for (unsigned i = 0; i < self->language->external_token_count; i++) {
    if (self->language->external_scanner.symbol_map[i] == self->language->keyword_capture_token) {
      st->retry_internal = false;
      break;
    }
  }
}

static inline void ts_parser__defer_failed_internal(TSParser *self, LexLoopState *st) {
  st->internal_already_failed = true;
  st->failed_internal_position = self->lexer.current_position;
  st->failed_internal_token_start = self->lexer.token_start_position;
  st->failed_internal_column_data = self->lexer.column_data;
  ts_lexer_reset(&self->lexer, st->start_position);
}

static inline bool ts_parser__advance_skipped_error(TSParser *self, LexLoopState *st) {
  if (!st->skipped_error) {
    LOG("skip_unrecognized_character");
    st->skipped_error = true;
    st->error_start_position = self->lexer.token_start_position;
    st->error_end_position = self->lexer.token_start_position;
    st->first_error_character = self->lexer.data.lookahead;
  }
  if (self->lexer.current_position.bytes == st->error_end_position.bytes) {
    if (self->lexer.data.eof(&self->lexer.data)) {
      self->lexer.data.result_symbol = ts_builtin_sym_error;
      return true;
    }
    self->lexer.data.advance(&self->lexer.data, false);
    if (self->exceeded_max_operations) return false;
  }
  st->error_end_position = self->lexer.current_position;
  return false;
}

static inline LookaheadToken ts_parser__finish_lexed_token(
  TSParser *self,
  StackVersion version,
  TSStateId parse_state,
  const LexLoopState *st
) {
  LookaheadToken result = {.parse_state = parse_state};
  if (st->skipped_error) {
    Length padding = length_sub(st->error_start_position, st->start_position);
    Length size = length_sub(st->error_end_position, st->error_start_position);
    result.subtree = ts_subtree_new_error(
      &self->tree_pool, st->first_error_character, padding, size, self->language
    );
    LOG_LOOKAHEAD(
      SYM_NAME(ts_subtree_symbol(result.subtree)),
      ts_subtree_total_size(result.subtree).bytes
    );
    return result;
  }

  bool is_keyword = false;
  bool is_keyword_exact = false;
  TSSymbol result_symbol = self->lexer.data.result_symbol;
  bool has_decode_error =
    !st->found_external_token &&
    self->lexer.token_has_decode_error &&
    result_symbol != ts_builtin_sym_end;
  Length padding = length_sub(self->lexer.token_start_position, st->start_position);
  Length size = length_sub(self->lexer.token_end_position, self->lexer.token_start_position);

  if (st->found_external_token) {
    result_symbol = self->language->external_scanner.symbol_map[result_symbol];
  } else if (
    !has_decode_error &&
    result_symbol == self->language->keyword_capture_token &&
    result_symbol != 0
  ) {
    if (!ts_parser__resolve_keyword(
      self, version, parse_state, &result_symbol, &is_keyword, &is_keyword_exact
    )) {
      return NULL_LOOKAHEAD;
    }
  }

  result.subtree = has_decode_error
    ? ts_parser__new_decode_error_token(&self->tree_pool, result_symbol, padding, size, self->language)
    : ts_subtree_new_leaf(
        &self->tree_pool,
        result_symbol,
        padding,
        size,
        st->found_external_token,
        st->found_external_token ? st->new_scanner_state_id : 0,
        self->language
      );
  result.is_keyword = is_keyword;
  result.is_keyword_exact = is_keyword_exact;
  result.has_external_scanner_state_change =
    st->found_external_token && st->external_scanner_state_changed;

  LOG_LOOKAHEAD(
    SYM_NAME(ts_subtree_symbol(result.subtree)),
    ts_subtree_total_size(result.subtree).bytes
  );
  return result;
}

static inline bool ts_parser__resolve_keyword(
  TSParser *self,
  StackVersion version,
  TSStateId parse_state,
  TSSymbol *symbol,
  bool *is_keyword,
  bool *is_keyword_exact
) {
  uint32_t end_byte = self->lexer.token_end_position.bytes;
  TSSymbol candidate = 0;
  if (!ts_parser__lookup_keyword(self, is_keyword, &candidate)) {
    ColumnCheckpoint end_column_checkpoint = self->lexer.end_column_checkpoint;
    ts_lexer_reset(&self->lexer, self->lexer.token_start_position);
    ts_lexer_start(&self->lexer);
    *is_keyword = ts_lexer_lex_table(&self->lexer, self->language->keyword_lex_table, 0);
    if (self->exceeded_max_operations) return false;
    self->lexer.end_column_checkpoint = end_column_checkpoint;
    if (*is_keyword && self->lexer.token_end_position.bytes == end_byte) {
      candidate = self->lexer.data.result_symbol;
    }
  }
  if (candidate && ts_stack_after_extra(self->stack, version) &&
      ts_language_symbol_metadata(self->language, candidate).immediate) {
    candidate = 0;
  }
  *is_keyword_exact = candidate != 0;
  if (!candidate) return true;
  bool keyword_is_valid =
    ts_table_cache_has_actions(self->table_cache, self->language, parse_state, candidate) ||
    ts_language_is_reserved_word(self->language, parse_state, candidate);
  if (keyword_is_valid ||
      (!ts_table_cache_has_actions(self->table_cache, self->language, parse_state, *symbol) &&
       ts_parser__recovery_accepts_keyword(self, version, parse_state, candidate))) {
    *is_keyword = keyword_is_valid;
    *symbol = candidate;
  }
  return true;
}

static bool ts_parser__recovery_accepts_keyword(
  TSParser *self,
  StackVersion version,
  TSStateId parse_state,
  TSSymbol candidate
) {
  if (parse_state != ERROR_STATE) {
    ts_stack_record_summary(self->stack, version, MAX_SUMMARY_DEPTH);
  }
  StackSummary *summary = ts_stack_get_summary(self->stack, version);
  if (!summary) return false;
  for (unsigned i = 0; i < summary->size; i++) {
    TSStateId state = array_get(summary, i)->state;
    if (state != ERROR_STATE && ts_table_cache_has_actions(self->table_cache, self->language, state, candidate)) {
      return true;
    }
  }
  return false;
}

static Subtree ts_parser__new_decode_error_token(
  SubtreePool *pool,
  TSSymbol symbol,
  Length padding,
  Length size,
  const TSLanguage *language
) {
  Subtree unexpected = ts_subtree_new_error(pool, TS_DECODE_ERROR, padding, size, language);
  SubtreeHeapData *error_data = ts_subtree_pool_alloc_node(pool, 1);
  ((Subtree *)error_data)[-1] = unexpected;
  MutableSubtree error_node = ts_subtree_finish_node(
    error_data, ts_builtin_sym_error, 1, 0, language
  );
  SubtreeHeapData *token_data = ts_subtree_pool_alloc_node(pool, 1);
  ((Subtree *)token_data)[-1] = ts_subtree_from_mut(error_node);
  MutableSubtree token_node = ts_subtree_finish_node(
    token_data, symbol, 1, 0, language
  );
  return ts_subtree_from_mut(token_node);
}

static void scanner_state_table_grow(ScannerStateTable *self);
static uint32_t scanner_state_bucket_find(
  const ScannerStateTable *self,
  uint32_t slot,
  uint64_t hash,
  const char *data,
  uint32_t length
);
static void scanner_state_bucket_insert(
  ScannerStateTable *self,
  uint32_t slot,
  uint32_t new_id
);
static int scanner_state_compare_key(
  const ScannerStateTable *self,
  uint32_t entry_id,
  uint64_t hash,
  const char *data,
  uint32_t length
);
static uint32_t scanner_state_aa_skew(ScannerStateEntry *entries, uint32_t node);
static uint32_t scanner_state_aa_split(ScannerStateEntry *entries, uint32_t node);
static uint64_t scanner_state_hash(uint64_t seed, const char *data, uint32_t length);
static uint64_t scanner_state_mix64(uint64_t x);
static inline uint64_t scanner_state_load_le64(const uint8_t *p);

static uint32_t scanner_state_table_intern(
  ScannerStateTable *self,
  const char *data,
  uint32_t length
) {
  if (length == 0) return 0;
  if (self->entries.size >= self->buckets.size) {
    scanner_state_table_grow(self);
  }
  uint64_t hash = scanner_state_hash(self->seed, data, length);
  uint32_t slot = (uint32_t)hash & (self->buckets.size - 1);
  uint32_t existing = scanner_state_bucket_find(self, slot, hash, data, length);
  if (existing) return existing;

  uint32_t offset = self->bytes.size;
  array_extend(&self->bytes, length, data);
  array_push(&self->entries, ((ScannerStateEntry) {
    .hash = hash,
    .offset = offset,
    .length = length,
    .left = 0,
    .right = 0,
    .level = 1,
  }));
  uint32_t new_id = self->entries.size;
  scanner_state_bucket_insert(self, slot, new_id);
  return new_id;
}

static void scanner_state_table_grow(ScannerStateTable *self) {
  if (self->buckets.size >= (1u << 30)) return;
  uint32_t old_size = self->buckets.size;
  uint32_t capacity = old_size ? old_size * 2 : 32;
  if (old_size > 0) {
    memset(self->buckets.contents, 0, old_size * sizeof(uint32_t));
  }
  array_grow_by(&self->buckets, capacity - old_size);
  uint32_t mask = capacity - 1;
  for (uint32_t i = 0; i < self->entries.size; i++) {
    ScannerStateEntry *entry = &self->entries.contents[i];
    entry->left = 0;
    entry->right = 0;
    entry->level = 1;
    scanner_state_bucket_insert(self, (uint32_t)entry->hash & mask, i + 1);
  }
}

static uint32_t scanner_state_bucket_find(
  const ScannerStateTable *self,
  uint32_t slot,
  uint64_t hash,
  const char *data,
  uint32_t length
) {
  uint32_t cur = self->buckets.contents[slot];
  while (cur) {
    int cmp = scanner_state_compare_key(self, cur, hash, data, length);
    if (cmp == 0) return cur;
    const ScannerStateEntry *entry = &self->entries.contents[cur - 1];
    cur = cmp < 0 ? entry->left : entry->right;
  }
  return 0;
}

static void scanner_state_bucket_insert(
  ScannerStateTable *self,
  uint32_t slot,
  uint32_t new_id
) {
  uint32_t root = self->buckets.contents[slot];
  if (!root) {
    self->buckets.contents[slot] = new_id;
    return;
  }
  ScannerStateEntry *entries = self->entries.contents;
  const ScannerStateEntry *target = &entries[new_id - 1];
  const char *target_data = self->bytes.contents + target->offset;
  uint32_t path[64];
  uint8_t go_right[64];
  uint32_t depth = 0;
  uint32_t cur = root;
  while (cur) {
    ts_assert(depth < 64);
    path[depth] = cur;
    bool right = scanner_state_compare_key(
      self, cur, target->hash, target_data, target->length
    ) > 0;
    go_right[depth] = (uint8_t)right;
    depth++;
    cur = right ? entries[cur - 1].right : entries[cur - 1].left;
  }
  uint32_t child = new_id;
  while (depth > 0) {
    uint32_t node = path[--depth];
    if (go_right[depth]) entries[node - 1].right = child;
    else entries[node - 1].left = child;
    child = scanner_state_aa_split(entries, scanner_state_aa_skew(entries, node));
  }
  self->buckets.contents[slot] = child;
}

static int scanner_state_compare_key(
  const ScannerStateTable *self,
  uint32_t entry_id,
  uint64_t hash,
  const char *data,
  uint32_t length
) {
  const ScannerStateEntry *entry = &self->entries.contents[entry_id - 1];
  if (hash != entry->hash) return hash < entry->hash ? -1 : 1;
  if (length != entry->length) return length < entry->length ? -1 : 1;
  return memcmp(data, self->bytes.contents + entry->offset, length);
}

static uint32_t scanner_state_aa_skew(ScannerStateEntry *entries, uint32_t node) {
  uint32_t left = entries[node - 1].left;
  if (left && entries[left - 1].level == entries[node - 1].level) {
    entries[node - 1].left = entries[left - 1].right;
    entries[left - 1].right = node;
    return left;
  }
  return node;
}

static uint32_t scanner_state_aa_split(ScannerStateEntry *entries, uint32_t node) {
  uint32_t right = entries[node - 1].right;
  if (right) {
    uint32_t right_right = entries[right - 1].right;
    if (right_right && entries[right_right - 1].level == entries[node - 1].level) {
      entries[node - 1].right = entries[right - 1].left;
      entries[right - 1].left = node;
      entries[right - 1].level++;
      return right;
    }
  }
  return node;
}

#define SIP_ROTL64(x, b) (((x) << (b)) | ((x) >> (64 - (b))))

#define SIP_ROUND(v0, v1, v2, v3) \
  do {                            \
    (v0) += (v1);                 \
    (v1) = SIP_ROTL64((v1), 13);  \
    (v1) ^= (v0);                 \
    (v0) = SIP_ROTL64((v0), 32);  \
    (v2) += (v3);                 \
    (v3) = SIP_ROTL64((v3), 16);  \
    (v3) ^= (v2);                 \
    (v0) += (v3);                 \
    (v3) = SIP_ROTL64((v3), 21);  \
    (v3) ^= (v0);                 \
    (v2) += (v1);                 \
    (v1) = SIP_ROTL64((v1), 17);  \
    (v1) ^= (v2);                 \
    (v2) = SIP_ROTL64((v2), 32);  \
  } while (0)

static uint64_t scanner_state_hash(uint64_t seed, const char *data, uint32_t length) {
  uint64_t k0 = seed;
  uint64_t k1 = scanner_state_mix64(seed ^ UINT64_C(0x646f72616e646f6d));
  uint64_t v0 = k0 ^ UINT64_C(0x736f6d6570736575);
  uint64_t v1 = k1 ^ UINT64_C(0x646f72616e646f6d);
  uint64_t v2 = k0 ^ UINT64_C(0x6c7967656e657261);
  uint64_t v3 = k1 ^ UINT64_C(0x7465646279746573);
  const uint8_t *bytes = (const uint8_t *)data;
  uint32_t full_len = length & ~7u;
  for (uint32_t i = 0; i < full_len; i += 8) {
    uint64_t m = scanner_state_load_le64(bytes + i);
    v3 ^= m;
    SIP_ROUND(v0, v1, v2, v3);
    v0 ^= m;
  }
  uint64_t tail = (uint64_t)length << 56;
  for (uint32_t i = full_len; i < length; i++) {
    tail |= (uint64_t)bytes[i] << ((i - full_len) * 8);
  }
  v3 ^= tail;
  SIP_ROUND(v0, v1, v2, v3);
  v0 ^= tail;
  v2 ^= 0xff;
  SIP_ROUND(v0, v1, v2, v3);
  SIP_ROUND(v0, v1, v2, v3);
  SIP_ROUND(v0, v1, v2, v3);
  return v0 ^ v1 ^ v2 ^ v3;
}

#undef SIP_ROUND
#undef SIP_ROTL64

static inline bool ts_parser__token_in_single_included_range(
  const Lexer *lexer,
  uint32_t start_byte,
  uint32_t end_byte
);

static inline bool ts_parser__lookup_keyword(
  TSParser *self,
  bool *is_keyword,
  TSSymbol *candidate
) {
  uint32_t start_byte = self->lexer.token_start_position.bytes;
  uint32_t end_byte = self->lexer.token_end_position.bytes;
  if (
    !self->language->keyword_lex_table ||
    self->lexer.logger.log ||
    !self->lexer.chunk ||
    start_byte < self->lexer.chunk_start ||
    end_byte < start_byte ||
    end_byte - self->lexer.chunk_start > self->lexer.chunk_size ||
    !ts_parser__token_in_single_included_range(&self->lexer, start_byte, end_byte)
  ) return false;

  const uint8_t *bytes =
    (const uint8_t *)self->lexer.chunk + (start_byte - self->lexer.chunk_start);
  TSLexKeywordResult kw = ts_lexer_lookup_keyword(
    &self->lexer,
    self->language->keyword_lex_table,
    bytes,
    end_byte - start_byte,
    self->lexer.input.encoding
  );
  if (!kw.handled) return false;
  *is_keyword = kw.is_keyword;
  *candidate = kw.candidate;
  return true;
}

static inline bool ts_parser__token_in_single_included_range(
  const Lexer *lexer,
  uint32_t start_byte,
  uint32_t end_byte
) {
  if (lexer->included_range_count == 1) {
    TSRange range = lexer->included_ranges[0];
    return start_byte >= range.start_byte && end_byte <= range.end_byte;
  }
  uint32_t low = 0, high = lexer->included_range_count;
  while (low < high) {
    uint32_t mid = low + (high - low) / 2;
    if (lexer->included_ranges[mid].end_byte < end_byte) low = mid + 1;
    else high = mid;
  }
  return low < lexer->included_range_count && start_byte >= lexer->included_ranges[low].start_byte;
}

static inline TSLexerMode ts_parser__lex_mode_for_extra_context(TSLexerMode mode, bool after_extra) {
  if (after_extra) {
    mode.lex_state = mode.lex_state_after_extra;
    mode.external_lex_state = mode.external_lex_state_after_extra;
  }
  return mode;
}

static bool ts_parser__token_shift_reduces_on(TSParser *self, TSStateId state, TSSymbol lookahead);

static bool ts_parser__eof_token_is_usable(
  TSParser *self,
  StackVersion version,
  TSSymbol lookahead
) {
  TSStateId current_state = ts_stack_state(self->stack, version);
  if (ts_table_cache_has_actions(self->table_cache, self->language, current_state, lookahead)) return true;
  ts_stack_record_summary(self->stack, version, MAX_SUMMARY_DEPTH);
  StackSummary *summary = ts_stack_get_summary(self->stack, version);
  for (unsigned i = 0; i < summary->size; i++) {
    TSStateId state = array_get(summary, i)->state;
    if (state == ERROR_STATE) continue;
    TSStateId marker_state = ts_table_cache_next_state(self->table_cache, self->language, state, lookahead);
    if (marker_state != 0 && marker_state != state &&
        ts_table_cache_has_actions(self->table_cache, self->language, marker_state, ts_builtin_sym_end)) {
      return true;
    }
    if (ts_parser__token_shift_reduces_on(self, state, lookahead)) return true;
  }
  return false;
}

// Whether shifting some token from `state` reaches a state that reduces on `lookahead`.
static bool ts_parser__token_shift_reduces_on(TSParser *self, TSStateId state, TSSymbol lookahead) {
  LookaheadIterator iterator = ts_language_lookaheads(self->language, state);
  while (ts_lookahead_iterator__next(&iterator)) {
    if (iterator.symbol == 0 || iterator.symbol >= self->language->token_count) continue;
    TSStateId next_state = ts_table_cache_next_state(self->table_cache, self->language, state, iterator.symbol);
    if (next_state != 0 && next_state != state &&
        ts_language_has_reduce_action(self->language, next_state, lookahead)) {
      return true;
    }
  }
  return false;
}

static LookaheadToken ts_parser__get_cached_token(
  TSParser *self,
  StackVersion version,
  TSStateId state,
  size_t position,
  uint32_t scanner_state_id,
  TableEntry *table_entry
) {
  TokenCache *cache = &self->token_cache;
  if (
    cache->byte_index == position &&
    cache->token.subtree.ptr &&
    cache->scanner_state_id == scanner_state_id
  ) {
    ts_table_cache_entry(self->table_cache, self->language, state, ts_subtree_symbol(cache->token.subtree), table_entry);
    if (ts_parser__can_reuse_cached_token(self, version, state, cache->token, cache->after_extra, table_entry)) {
      ts_subtree_retain(cache->token.subtree);
      return cache->token;
    }
  }
  return NULL_LOOKAHEAD;
}

static void ts_parser__set_cached_token(
  TSParser *self,
  uint32_t byte_index,
  uint32_t scanner_state_id,
  LookaheadToken token,
  bool after_extra
) {
  TokenCache *cache = &self->token_cache;
  if (token.subtree.bits != cache->token.subtree.bits) {
    if (token.subtree.ptr) ts_subtree_retain(token.subtree);
    if (cache->token.subtree.ptr) ts_subtree_release(&self->tree_pool, cache->token.subtree);
  }
  cache->token = token;
  cache->scanner_state_id = scanner_state_id;
  cache->byte_index = byte_index;
  cache->after_extra = after_extra;
}

static void ts_parser__clear_cached_token(TSParser *self) {
  if (self->token_cache.token.subtree.ptr) {
    ts_subtree_release(&self->tree_pool, self->token_cache.token.subtree);
  }
  self->token_cache = (TokenCache) {.token = NULL_LOOKAHEAD};
}

// Determine if a given tree should be replaced by an alternative tree.
//
// The decision is based on the trees' error costs (if any), their dynamic precedence,
// and finally, as a default, by a recursive comparison of the trees' symbols.
static bool ts_parser__select_tree(TSParser *self, Subtree left, Subtree right) {
  if (!left.ptr) return true;
  if (!right.ptr) return false;

  if (ts_subtree_error_cost(right) < ts_subtree_error_cost(left)) {
    LOG("select_smaller_error symbol:%s, over_symbol:%s", TREE_NAME(right), TREE_NAME(left));
    return true;
  }

  if (ts_subtree_error_cost(left) < ts_subtree_error_cost(right)) {
    LOG("select_smaller_error symbol:%s, over_symbol:%s", TREE_NAME(left), TREE_NAME(right));
    return false;
  }

  if (ts_subtree_dynamic_precedence(right) > ts_subtree_dynamic_precedence(left)) {
    LOG("select_higher_precedence symbol:%s, prec:%" PRId32 ", over_symbol:%s, other_prec:%" PRId32,
        TREE_NAME(right), ts_subtree_dynamic_precedence(right), TREE_NAME(left),
        ts_subtree_dynamic_precedence(left));
    return true;
  }

  if (ts_subtree_dynamic_precedence(left) > ts_subtree_dynamic_precedence(right)) {
    LOG("select_higher_precedence symbol:%s, prec:%" PRId32 ", over_symbol:%s, other_prec:%" PRId32,
        TREE_NAME(left), ts_subtree_dynamic_precedence(left), TREE_NAME(right),
        ts_subtree_dynamic_precedence(right));
    return false;
  }

  if (ts_subtree_error_cost(left) > 0) return true;

  int comparison = ts_subtree_compare(left, right, &self->tree_pool);
  switch (comparison) {
    case -1:
      LOG("select_earlier symbol:%s, over_symbol:%s", TREE_NAME(left), TREE_NAME(right));
      return false;
      break;
    case 1:
      LOG("select_earlier symbol:%s, over_symbol:%s", TREE_NAME(right), TREE_NAME(left));
      return true;
    default:
      LOG("select_existing symbol:%s, over_symbol:%s", TREE_NAME(left), TREE_NAME(right));
      return false;
  }
}

// Determine if a given tree's children should be replaced by an alternative
// array of children.
static bool ts_parser__select_children(
  TSParser *self,
  Subtree left,
  const SubtreeArray *children
) {
  array_assign(&self->scratch_trees, children);

  MutableSubtree scratch_tree = ts_subtree_init_scratch_node(
    ts_subtree_symbol(left),
    &self->scratch_trees,
    0,
    self->language
  );

  return ts_parser__select_tree(
    self,
    left,
    ts_subtree_from_mut(scratch_tree)
  );
}

static void ts_parser__shift(
  TSParser *self,
  StackVersion version,
  TSStateId state,
  Subtree lookahead,
  bool extra
) {
  ts_assert(
    ts_subtree_child_count(lookahead) == 0 ||
    (ts_subtree_child_count(lookahead) == 1 &&
     ts_subtree_is_error(ts_subtree_children(lookahead)[0]))
  );
  Subtree subtree_to_push = lookahead;
  if (extra != ts_subtree_extra(lookahead)) {
    MutableSubtree result = ts_subtree_make_mut(&self->tree_pool, lookahead);
    ts_subtree_set_extra(&result, extra);
    subtree_to_push = ts_subtree_from_mut(result);
  }

  ts_stack_push(self->stack, version, subtree_to_push, state);
  if (ts_subtree_has_external_tokens(subtree_to_push)) {
    ts_stack_set_external_scanner_state_id(
      self->stack, version, ts_subtree_scanner_state_id(subtree_to_push)
    );
  }
}

static void ts_parser__confirm_incorporated_recovery(TSParser *self, StackVersion version, const SubtreeArray *children, bool normal_reduction);
static uint32_t ts_parser__discard_overflow_slices(
  TSParser *self, const StackSliceArray *pop, uint32_t index, StackVersion slice_version
);
static MutableSubtree ts_parser__build_slice_parent(
  TSParser *self, const StackSliceArray *pop, uint32_t *index,
  StackVersion slice_version, TSSymbol symbol, uint16_t production_id
);
static void ts_parser__push_reduced_node(
  TSParser *self,
  StackVersion slice_version,
  TSSymbol symbol,
  MutableSubtree parent,
  bool is_extra
);

static bool ts_parser__reduce_in_place(
  TSParser *self,
  StackVersion version,
  ReduceAction action
) {
  uint32_t child_count;
  SubtreeHeapData *data = ts_stack_pop_count_in_place(
    self->stack, version, action.count, &child_count, &self->trailing_extras
  );
  if (!data) return false;
  SubtreeArray children = {.contents = (Subtree *)data - child_count, .size = child_count};
  ts_parser__confirm_incorporated_recovery(self, version, &children, true);
  MutableSubtree parent = ts_subtree_finish_node(
    data, action.symbol, child_count, action.production_id, self->language
  );
  parent.ptr->dynamic_precedence += action.dynamic_precedence;
  ts_parser__push_reduced_node(self, version, action.symbol, parent, action.is_extra);
  return true;
}

// code-gauge-allow functionParameterCount -- the parse loop passes every property of the reduction action
static StackVersion ts_parser__reduce(
  TSParser *self,
  StackVersion version,
  TSSymbol symbol,
  uint32_t count,
  int dynamic_precedence,
  uint16_t production_id,
  bool is_fragile,
  bool is_extra
) {
  uint32_t initial_version_count = ts_stack_version_count(self->stack);
  ReduceAction action = {
    .count = count,
    .symbol = symbol,
    .dynamic_precedence = dynamic_precedence,
    .production_id = production_id,
    .is_extra = is_extra,
  };
  if (!is_fragile && ts_parser__reduce_in_place(self, version, action)) {
    return version;
  }

  StackReductionStatus reduction_status;
  StackSliceArray pop = ts_stack_pop_count_for_reduce(
    self->stack, version, count, MAX_VERSION_COUNT + MAX_VERSION_COUNT_OVERFLOW,
    &reduction_status
  );
  ts_parser__log_version_limit(self, "reduce_paths", version, reduction_status.was_truncated);
  uint32_t removed_version_count = 0;
  uint64_t version_limit = (uint64_t)MAX_VERSION_COUNT + MAX_VERSION_COUNT_OVERFLOW + ts_stack_halted_version_count(self->stack);
  for (uint32_t i = 0; i < pop.size; i++) {
    StackSlice slice = *array_get(&pop, i);
    StackVersion slice_version = slice.version - removed_version_count;
    if ((uint64_t)slice_version > version_limit) {
      i = ts_parser__discard_overflow_slices(self, &pop, i, slice_version);
      removed_version_count++;
      continue;
    }

    MutableSubtree parent = ts_parser__build_slice_parent(
      self, &pop, &i, slice_version, symbol, production_id
    );
    parent.ptr->dynamic_precedence += dynamic_precedence;
    ts_parser__push_reduced_node(self, slice_version, symbol, parent, is_extra);

    for (StackVersion j = 0; j < slice_version; j++) {
      if (j != version && ts_stack_merge(self->stack, j, slice_version)) {
        removed_version_count++;
        break;
      }
    }
  }

  return ts_stack_version_count(self->stack) > initial_version_count
    ? initial_version_count
    : STACK_VERSION_NONE;
}

static uint32_t ts_parser__discard_overflow_slices(
  TSParser *self,
  const StackSliceArray *pop,
  uint32_t index,
  StackVersion slice_version
) {
  StackSlice slice = *array_get(pop, index);
  ts_parser__log_version_limit(self, "reduce", slice_version, true);
  ts_stack_remove_version(self->stack, slice_version);
  ts_subtree_array_delete(&self->tree_pool, &slice.subtrees);
  while (index + 1 < pop->size) {
    LOG("aborting reduce with too many versions")
    StackSlice next_slice = *array_get(pop, index + 1);
    if (next_slice.version != slice.version) break;
    ts_subtree_array_delete(&self->tree_pool, &next_slice.subtrees);
    index++;
  }
  return index;
}

static MutableSubtree ts_parser__build_slice_parent(
  TSParser *self,
  const StackSliceArray *pop,
  uint32_t *index,
  StackVersion slice_version,
  TSSymbol symbol,
  uint16_t production_id
) {
  StackSlice slice = *array_get(pop, *index);
  SubtreeArray children = slice.subtrees;
  ts_subtree_array_remove_trailing_extras(&children, &self->trailing_extras);
  ts_parser__confirm_incorporated_recovery(self, slice_version, &children, true);
  MutableSubtree parent = ts_subtree_new_node(&self->tree_pool, symbol, &children, production_id, self->language);

  while (*index + 1 < pop->size) {
    StackSlice next_slice = *array_get(pop, *index + 1);
    if (next_slice.version != slice.version) break;
    (*index)++;

    SubtreeArray next_slice_children = next_slice.subtrees;
    ts_subtree_array_remove_trailing_extras(&next_slice_children, &self->trailing_extras2);
    if (ts_parser__select_children(self, ts_subtree_from_mut(parent), &next_slice_children)) {
      ts_subtree_array_clear(&self->tree_pool, &self->trailing_extras);
      ts_subtree_release(&self->tree_pool, ts_subtree_from_mut(parent));
      array_swap(&self->trailing_extras, &self->trailing_extras2);
      parent = ts_subtree_new_node(&self->tree_pool, symbol, &next_slice_children, production_id, self->language);
    } else {
      array_clear(&self->trailing_extras2);
      ts_subtree_array_delete(&self->tree_pool, &next_slice.subtrees);
    }
  }
  return parent;
}

static void ts_parser__push_reduced_node(
  TSParser *self,
  StackVersion slice_version,
  TSSymbol symbol,
  MutableSubtree parent,
  bool is_extra
) {
  TSStateId state = ts_stack_state(self->stack, slice_version);
  TSStateId next_state = ts_table_cache_next_state(self->table_cache, self->language, state, symbol);
  if (is_extra) {
    next_state = state;
    ts_subtree_heap_set_flag(parent.ptr, SUBTREE_FLAG_EXTRA, true);
  }

  ts_stack_push(self->stack, slice_version, ts_subtree_from_mut(parent), next_state);
  for (uint32_t j = 0; j < self->trailing_extras.size; j++) {
    ts_stack_push(self->stack, slice_version, *array_get(&self->trailing_extras, j), next_state);
  }
}

static void ts_parser__confirm_incorporated_recovery(
  TSParser *self,
  StackVersion version,
  const SubtreeArray *children,
  bool normal_reduction
) {
  if (!ts_stack_has_unconfirmed_missing_token(self->stack, version) &&
      !ts_stack_unconfirmed_recovery(self->stack, version)) return;
  for (uint32_t i = 0; i < children->size; i++) {
    Subtree child = *array_get(children, i);
    if (ts_subtree_missing(child)) {
      ts_stack_set_unconfirmed_missing_token(self->stack, version, false);
    }
    // A recovered expression can reduce while its enclosing block remains unfinished.
    if (normal_reduction && ts_stack_state(self->stack, version) == 1 && ts_subtree_error_cost(child) > 0) {
      ts_stack_set_unconfirmed_recovery(self->stack, version, false);
    }
  }
}

static void ts_parser__accept(
  TSParser *self,
  StackVersion version,
  Subtree lookahead
) {
  ts_assert(ts_subtree_is_eof(lookahead));
  ts_assert(self->accept_version == STACK_VERSION_NONE);
  ts_stack_push(self->stack, version, lookahead, 1);
  self->accept_version = version;
  self->accept_index = 0;
  self->accept_collected = false;
}

static bool ts_parser__check_accept_progress(void *, unsigned);
static Subtree ts_parser__root_from_stack(TSParser *, SubtreeArray *);

static bool ts_parser__finish_accept(TSParser *self) {
  if (!self->accept_collected && !ts_stack_pop_all(
    self->stack, self->accept_version, &self->accept_slices,
    ts_parser__check_accept_progress, self
  )) return false;
  self->accept_collected = true;
  while (self->accept_index < self->accept_slices.size) {
    StackSlice *slice = array_get(&self->accept_slices, self->accept_index);
    SubtreeArray trees = slice->subtrees;

    Subtree root = ts_parser__root_from_stack(self, &trees);

    slice->subtrees = (SubtreeArray) array_new();
    self->accept_index++;
    ts_assert(root.ptr);
    self->accept_count++;

    if (!self->finished_tree.ptr || ts_parser__select_tree(self, self->finished_tree, root)) {
      if (self->finished_tree.ptr) ts_subtree_release(&self->tree_pool, self->finished_tree);
      self->finished_tree = root;
    } else {
      ts_subtree_release(&self->tree_pool, root);
    }
    if (!ts_parser__check_accept_progress(self, trees.size)) return false;
  }

  ts_stack_remove_version(self->stack, array_get(&self->accept_slices, 0)->version);
  ts_stack_halt(self->stack, self->accept_version);
  self->accept_version = STACK_VERSION_NONE;
  self->accept_slices = (StackSliceArray) array_new();
  return true;
}

static Subtree ts_parser__root_from_stack(TSParser *self, SubtreeArray *trees) {
  for (uint32_t j = trees->size - 1; j + 1 > 0; j--) {
    Subtree tree = *array_get(trees, j);
    if (!ts_subtree_extra(tree)) {
      ts_assert(!ts_subtree_is_inline(tree));
      uint32_t child_count = ts_subtree_child_count(tree);
      const Subtree *children = ts_subtree_children(tree);
      for (uint32_t k = 0; k < child_count; k++) {
        ts_subtree_retain(children[k]);
      }
      array_splice(trees, j, 1, child_count, children);
      Subtree root = ts_subtree_from_mut(ts_subtree_new_node(
        &self->tree_pool,
        ts_subtree_symbol(tree),
        trees,
        ts_subtree_production_id(tree),
        self->language
      ));
      ts_subtree_release(&self->tree_pool, tree);
      return root;
    }
  }
  return NULL_SUBTREE;
}

static bool ts_parser__process_candidate_recovery_actions(
  TSParser *self,
  const uint32_t *actions,
  uint32_t action_count
) {
  bool has_shift_action = false;
  for (uint32_t i = 0; i < action_count; i++) {
    TSParseAction action = ts_language_parse_action(self->language, actions[i]);
    switch (action.type) {
      case TSParseActionTypeShift:
      case TSParseActionTypeRecover:
        if (!action.shift.extra) has_shift_action = true;
        break;
      case TSParseActionTypeReduce:
      case TSParseActionTypeReduceExtra:
        if (action.reduce.child_count > 0)
          ts_reduce_action_set_add(&self->reduce_actions, (ReduceAction) {
            .symbol = action.reduce.symbol,
            .count = action.reduce.child_count,
            .dynamic_precedence = action.reduce.dynamic_precedence,
            .production_id = action.reduce.production_id,
            .is_extra = action.type == TSParseActionTypeReduceExtra,
          });
        break;
      default:
        break;
    }
  }
  return has_shift_action;
}

static bool ts_parser__do_all_potential_reductions(
  TSParser *self,
  StackVersion starting_version,
  TSSymbol lookahead_symbol
) {
  uint32_t initial_version_count = ts_stack_version_count(self->stack);

  bool can_shift_lookahead_symbol = false;
  StackVersion version = starting_version;
  unsigned chained_reduction_count = 0;
  for (;;) {
    uint32_t version_count = ts_stack_version_count(self->stack);
    if (version >= version_count) break;

    bool merged = false;
    for (StackVersion j = initial_version_count; j < version; j++) {
      if (ts_stack_merge(self->stack, j, version)) {
        merged = true;
        break;
      }
    }
    if (merged) {
      chained_reduction_count = 0;
      continue;
    }

    TSStateId state = ts_stack_state(self->stack, version);
    bool has_shift_action = false;
    array_clear(&self->reduce_actions);

    if (lookahead_symbol != 0) {
      TableEntry entry;
      ts_table_cache_entry(self->table_cache, self->language, state, lookahead_symbol, &entry);
      has_shift_action = ts_parser__process_candidate_recovery_actions(self, entry.actions, entry.action_count);
    } else {
      LookaheadIterator iter = ts_language_lookaheads(self->language, state);
      while (ts_lookahead_iterator__next(&iter)) {
        // only terminal tokens are valid lookaheads for reduction decisions
        if (iter.symbol == ts_builtin_sym_end || iter.symbol >= self->language->token_count) continue;
        if (ts_parser__process_candidate_recovery_actions(self, iter.actions, iter.action_count))
          has_shift_action = true;
      }

      // The iterator's visiting order depends on how the table encodes the state's row. Reductions are
      // applied sequentially and the last reduction version survives, so sort them into an order that
      // does not depend on the encoding.
      for (uint32_t j = 1; j < self->reduce_actions.size; j++) {
        ReduceAction key = self->reduce_actions.contents[j];
        int32_t k = (int32_t)j - 1;
        while (k >= 0 && ts_reduce_action_precedes(key, self->reduce_actions.contents[k])) {
          self->reduce_actions.contents[k + 1] = self->reduce_actions.contents[k];
          k--;
        }
        self->reduce_actions.contents[k + 1] = key;
      }
    }

    StackVersion reduction_version = STACK_VERSION_NONE;
    for (uint32_t j = 0; j < self->reduce_actions.size; j++) {
      ReduceAction action = *array_get(&self->reduce_actions, j);

      reduction_version = ts_parser__reduce(
        self, version, action.symbol, action.count,
        action.dynamic_precedence, action.production_id,
        true, action.is_extra
      );
    }

    if (has_shift_action) {
      can_shift_lookahead_symbol = true;
    } else if (reduction_version != STACK_VERSION_NONE && chained_reduction_count < MAX_CHAINED_REDUCTION_COUNT) {
      ts_stack_renumber_version(self->stack, reduction_version, version);
      chained_reduction_count++;
      continue;
    } else if (lookahead_symbol != 0) {
      ts_stack_remove_version(self->stack, version);
    }

    chained_reduction_count = 0;
    if (version == starting_version) {
      version = version_count;
    } else {
      version++;
    }
  }

  return can_shift_lookahead_symbol;
}

static void ts_parser__prepend_popped_error(
  TSParser *self,
  StackVersion version,
  SubtreeArray *subtrees
);

static bool ts_parser__recover_to_state(
  TSParser *self,
  StackVersion version,
  unsigned depth,
  TSStateId goal_state
) {
  StackSliceArray pop = ts_stack_pop_count(self->stack, version, depth);
  StackVersion previous_version = STACK_VERSION_NONE;

  for (unsigned i = 0; i < pop.size; i++) {
    StackSlice slice = *array_get(&pop, i);

    if (slice.version == previous_version) {
      ts_subtree_array_delete(&self->tree_pool, &slice.subtrees);
      array_erase(&pop, i--);
      continue;
    }

    if (ts_stack_state(self->stack, slice.version) != goal_state) {
      ts_stack_halt(self->stack, slice.version);
      ts_subtree_array_delete(&self->tree_pool, &slice.subtrees);
      array_erase(&pop, i--);
      continue;
    }

    ts_parser__prepend_popped_error(self, slice.version, &slice.subtrees);
    ts_subtree_array_remove_trailing_extras(&slice.subtrees, &self->trailing_extras);
    // A missing token inside an ERROR no longer opens a construct; keeping it unconfirmed
    // would alter how ambiguities in unrelated code that follows are resolved.
    ts_parser__confirm_incorporated_recovery(self, slice.version, &slice.subtrees, false);

    if (slice.subtrees.size > 0) {
      Subtree error = ts_subtree_new_error_node(&self->tree_pool, &slice.subtrees, true, self->language);
      ts_stack_push_recovery(self->stack, slice.version, error, goal_state, self->language);
      ts_stack_set_unconfirmed_recovery(self->stack, slice.version, true);
    } else {
      array_delete(&slice.subtrees);
    }

    for (unsigned j = 0; j < self->trailing_extras.size; j++) {
      Subtree tree = *array_get(&self->trailing_extras, j);
      ts_stack_push(self->stack, slice.version, tree, goal_state);
    }

    previous_version = slice.version;
  }

  return previous_version != STACK_VERSION_NONE;
}

static void ts_parser__prepend_popped_error(
  TSParser *self,
  StackVersion version,
  SubtreeArray *subtrees
) {
  SubtreeArray error_trees = ts_stack_pop_error(self->stack, version);
  if (error_trees.size == 0) return;

  ts_assert(error_trees.size == 1);
  Subtree error_tree = *array_get(&error_trees, 0);
  uint32_t error_child_count = ts_subtree_child_count(error_tree);
  if (error_child_count > 0) {
    SubtreeArray nested = array_new();
    array_reserve(&nested, error_child_count);
    for (unsigned j = 0; j < error_child_count; j++) {
      Subtree child = ts_subtree_children(error_tree)[j];
      ts_subtree_retain(child);
      array_push(&nested, child);
    }
    Subtree nested_error = ts_subtree_from_mut(ts_subtree_new_node(
      &self->tree_pool, ts_builtin_sym_error_repeat, &nested, 0, self->language
    ));
    array_insert(subtrees, 0, nested_error);
  }
  ts_subtree_array_delete(&self->tree_pool, &error_trees);
}

static bool ts_parser__recover_to_entry(
  TSParser *self, StackVersion version, StackSummaryEntry *entry
) {
  if (ts_parser__recover_to_state(self, version, entry->depth, entry->state)) return true;
  StackSummary current = ts_stack_current_summary(self->stack, version, MAX_SUMMARY_DEPTH + 1);
  bool recovered = false;
  for (unsigned i = 0; i < current.size; i++) {
    StackSummaryEntry candidate = *array_get(&current, i);
    if (candidate.state == entry->state && candidate.position.bytes >= entry->position.bytes &&
        (!entry->has_root_subtree || candidate.has_root_subtree) && candidate.depth != entry->depth &&
        ts_parser__recover_to_state(self, version, candidate.depth, candidate.state)) {
      entry->depth = candidate.depth;
      recovered = true;
      break;
    }
  }
  array_delete(&current);
  return recovered;
}

static bool ts_parser__state_finishes_root(TSParser *self, TSStateId state);

static void ts_parser__recover(
  TSParser *self,
  StackVersion version,
  LookaheadToken lookahead,
  bool can_commit_keyword_recovery
) {
  bool did_recover = false;
  bool recovered_root = false;
  unsigned previous_version_count = ts_stack_version_count(self->stack);
  Length position = ts_stack_position(self->stack, version);
  StackSummary *summary = ts_stack_get_summary(self->stack, version);
  unsigned node_count_since_error = ts_stack_node_count_since_error(self->stack, version);
  unsigned current_error_cost = ts_stack_error_cost(self->stack, version);

  if (summary && !ts_subtree_is_error(lookahead.subtree)) {
    TSSymbol symbol = ts_subtree_symbol(lookahead.subtree);
    unsigned recovery_pass_count = lookahead.is_keyword &&
      symbol != self->language->keyword_capture_token ? 2 : 1;
    for (unsigned recovery_pass = 0; recovery_pass < recovery_pass_count && !did_recover; recovery_pass++) {
      TSSymbol recovery_symbol = recovery_pass == 0 ? symbol : self->language->keyword_capture_token;
      StackSummaryEntry recovery_entry;
      bool found_entry = false;
      bool reached_prefix = false;
      for (unsigned i = 0; i < summary->size && !reached_prefix; i++) {
        StackSummaryEntry entry = *array_get(summary, i);
        reached_prefix = entry.has_root_subtree && ts_parser__state_finishes_root(self, entry.state);

        if (entry.state == ERROR_STATE) continue;
        if (entry.position.bytes == position.bytes) continue;
        unsigned depth = entry.depth;
        if (node_count_since_error > 0) depth++;

        bool would_merge = false;
        for (unsigned j = 0; j < previous_version_count; j++) {
          if (
            ts_stack_state(self->stack, j) == entry.state &&
            ts_stack_position(self->stack, j).bytes == position.bytes
          ) {
            would_merge = true;
            break;
          }
        }
        if (would_merge) continue;

        unsigned new_cost =
          current_error_cost +
          entry.depth * ERROR_COST_PER_SKIPPED_TREE +
          (position.bytes - entry.position.bytes) * ERROR_COST_PER_SKIPPED_CHAR +
          (position.extent.row - entry.position.extent.row) * ERROR_COST_PER_SKIPPED_LINE;
        if (ts_parser__better_version_exists(self, version, false, new_cost)) break;

        if (
          ts_table_cache_has_actions(self->table_cache, self->language, entry.state, recovery_symbol) &&
          (recovery_pass == 0 || (
            !ts_table_cache_has_actions(self->table_cache, self->language, entry.state, symbol) &&
            !ts_language_is_reserved_word(self->language, entry.state, symbol)
          ))
        ) {
          entry.depth = depth;
          recovery_entry = entry;
          found_entry = true;
          break;
        } else if (recovery_pass == 0 && lookahead.is_keyword_exact) {
          reached_prefix = false;
        }
      }
      if (found_entry && ts_parser__recover_to_entry(self, version, &recovery_entry)) {
        did_recover = true;
        recovered_root = recovery_pass == 0 &&
          (recovery_entry.state == 1 || ts_parser__state_finishes_root(self, recovery_entry.state));
        LOG("recover_to_previous state:%u, depth:%u", recovery_entry.state, recovery_entry.depth);
        LOG_STACK();
      }
    }
  }

  for (unsigned i = previous_version_count; i < ts_stack_version_count(self->stack); i++) {
    if (!ts_stack_is_active(self->stack, i)) {
      LOG("removed paused version:%u", i);
      ts_stack_remove_version(self->stack, i--);
      LOG_STACK();
    }
  }

  if (ts_subtree_is_eof(lookahead.subtree)) {
    LOG("recover_eof");
    SubtreeArray children = array_new();
    Subtree parent = ts_subtree_new_error_node(&self->tree_pool, &children, false, self->language);
    ts_stack_push(self->stack, version, parent, 1);
    ts_parser__accept(self, version, lookahead.subtree);
    return;
  }

  if (did_recover && ts_stack_version_count(self->stack) > MAX_VERSION_COUNT) {
    ts_parser__log_version_limit(self, "recover", version, true);
    ts_stack_halt(self->stack, version);
    ts_subtree_release(&self->tree_pool, lookahead.subtree);
    return;
  }

  if (
    did_recover &&
    (lookahead.has_external_scanner_state_change ||
     (can_commit_keyword_recovery && recovered_root && lookahead.is_keyword_exact &&
      ts_subtree_symbol(lookahead.subtree) != self->language->keyword_capture_token))
  ) {
    ts_stack_halt(self->stack, version);
    ts_subtree_release(&self->tree_pool, lookahead.subtree);
    return;
  }

  // Do not recover if the result would clearly be worse than some existing stack version.
  unsigned new_cost =
    current_error_cost + ERROR_COST_PER_SKIPPED_TREE +
    ts_subtree_total_bytes(lookahead.subtree) * ERROR_COST_PER_SKIPPED_CHAR +
    ts_subtree_total_size(lookahead.subtree).extent.row * ERROR_COST_PER_SKIPPED_LINE;
  if (ts_parser__better_version_exists(self, version, false, new_cost)) {
    ts_stack_halt(self->stack, version);
    ts_subtree_release(&self->tree_pool, lookahead.subtree);
    return;
  }

  // If the current lookahead token is an extra token, mark it as extra. This means it won't
  // be counted in error cost calculations.
  unsigned n;
  const uint32_t *actions = ts_language_actions(self->language, 1, ts_subtree_symbol(lookahead.subtree), &n);
  if (n > 0 && (actions[n - 1] & TS_PARSE_ACTION_MASK) == SHIFT_EXTRA()) {
    MutableSubtree mutable_lookahead = ts_subtree_make_mut(&self->tree_pool, lookahead.subtree);
    ts_subtree_set_extra(&mutable_lookahead, true);
    lookahead.subtree = ts_subtree_from_mut(mutable_lookahead);
  }

  // Wrap the lookahead token in an ERROR.
  LOG("skip_token symbol:%s", TREE_NAME(lookahead.subtree));
  SubtreeArray children = array_new();
  array_reserve(&children, 1);
  array_push(&children, lookahead.subtree);
  MutableSubtree error_repeat = ts_subtree_new_node(
    &self->tree_pool,
    ts_builtin_sym_error_repeat,
    &children,
    0,
    self->language
  );

  // If other tokens have already been skipped, so there is already an ERROR at the top of the
  // stack, then pop that ERROR off the stack and wrap the two ERRORs together into one larger
  // ERROR.
  if (node_count_since_error > 0) {
    StackSliceArray pop = ts_stack_pop_count(self->stack, version, 1);

    // TODO: Figure out how to make this condition occur.
    // See https://github.com/atom/atom/issues/18450#issuecomment-439579778
    // If multiple stack versions have merged at this point, just pick one of the errors
    // arbitrarily and discard the rest.
    if (pop.size > 1) {
      for (unsigned i = 1; i < pop.size; i++) {
        ts_subtree_array_delete(&self->tree_pool, &array_get(&pop, i)->subtrees);
      }
      while (ts_stack_version_count(self->stack) > array_get(&pop, 0)->version + 1) {
        ts_stack_remove_version(self->stack, array_get(&pop, 0)->version + 1);
      }
    }

    ts_stack_renumber_version(self->stack, array_get(&pop, 0)->version, version);
    array_push(&array_get(&pop, 0)->subtrees, ts_subtree_from_mut(error_repeat));
    error_repeat = ts_subtree_new_node(
      &self->tree_pool,
      ts_builtin_sym_error_repeat,
      &array_get(&pop, 0)->subtrees,
      0,
      self->language
    );
  }

  ts_stack_push(self->stack, version, ts_subtree_from_mut(error_repeat), ERROR_STATE);
  if (ts_subtree_has_external_tokens(lookahead.subtree)) {
    ts_stack_set_external_scanner_state_id(
      self->stack, version, ts_subtree_scanner_state_id(lookahead.subtree)
    );
  }

  bool has_error = true;
  for (unsigned i = 0; i < ts_stack_version_count(self->stack); i++) {
    ErrorStatus status = ts_parser__version_status(self, i);
    if (!status.is_in_error) {
      has_error = false;
      break;
    }
  }
  self->has_error = has_error;
}

static bool ts_parser__state_finishes_root(TSParser *self, TSStateId state) {
  TableEntry entry;
  ts_table_cache_entry(self->table_cache, self->language, state, ts_builtin_sym_end, &entry);
  for (unsigned i = 0; i < entry.action_count; i++) {
    TSParseAction action = ts_language_parse_action(self->language, entry.actions[i]);
    if (action.type == TSParseActionTypeAccept) return true;
    if (action.type != TSParseActionTypeReduce || action.reduce.child_count != 1) continue;
    TSStateId next_state = ts_table_cache_next_state(self->table_cache, self->language, 1, action.reduce.symbol);
    TableEntry next_entry;
    ts_table_cache_entry(self->table_cache, self->language, next_state, ts_builtin_sym_end, &next_entry);
    for (unsigned j = 0; j < next_entry.action_count; j++) {
      if ((next_entry.actions[j] & TS_PARSE_ACTION_TYPE_MASK) == TSParseActionTypeAccept) return true;
    }
  }
  return false;
}

static TSSymbol ts_parser__sole_missing_token(TSParser *self, TSStateId state);

static bool ts_parser__insert_missing_token(
  TSParser *self,
  StackVersion version,
  Subtree lookahead,
  Length position,
  bool require_reduction
);

static bool ts_parser__missing_token_completes_recovery(TSParser *self, StackVersion version, StackVersion first_candidate);

static bool ts_parser__try_insert_missing_token(
  TSParser *self,
  StackVersion version,
  uint32_t previous_version_count,
  uint32_t version_count,
  Subtree lookahead,
  Length position
);

static void ts_parser__handle_error(
  TSParser *self,
  StackVersion version,
  LookaheadToken lookahead
) {
  uint32_t previous_version_count = ts_stack_version_count(self->stack);

  // Perform any reductions that can happen in this state, regardless of the lookahead. After
  // skipping one or more invalid tokens, the parser might find a token that would have allowed
  // a reduction to take place.
  ts_parser__do_all_potential_reductions(self, version, 0);
  uint32_t version_count = ts_stack_version_count(self->stack);
  Length position = ts_stack_position(self->stack, version);

  bool did_insert_missing_token = ts_parser__try_insert_missing_token(
    self, version, previous_version_count, version_count, lookahead.subtree, position
  );

  bool restored_root = did_insert_missing_token && lookahead.is_keyword_exact &&
    ts_parser__missing_token_completes_recovery(self, version, version_count);
  for (StackVersion v = version; v < version_count;) {
    if (restored_root) {
      ts_stack_halt(self->stack, v);
    } else {
      ts_stack_push(self->stack, v, NULL_SUBTREE, ERROR_STATE);
    }
    v = (v == version) ? previous_version_count : v + 1;
  }
  if (restored_root) {
    ts_subtree_release(&self->tree_pool, lookahead.subtree);
    return;
  }

  for (unsigned i = previous_version_count; i < version_count; i++) {
    bool did_merge = ts_stack_merge(self->stack, version, previous_version_count);
    ts_assert(did_merge);
  }

  ts_stack_record_summary(self->stack, version, MAX_SUMMARY_DEPTH);
  ts_parser__recover(self, version, lookahead, !did_insert_missing_token);

  LOG_STACK();
}

static bool ts_parser__try_insert_missing_token(
  TSParser *self,
  StackVersion version,
  uint32_t previous_version_count,
  uint32_t version_count,
  Subtree lookahead,
  Length position
) {
  for (unsigned pass = 0; pass < 2; pass++) {
    for (StackVersion v = version; v < version_count;) {
      if (ts_parser__insert_missing_token(self, v, lookahead, position, pass == 0)) {
        return true;
      }
      v = (v == version) ? previous_version_count : v + 1;
    }
  }
  return false;
}

static bool ts_parser__missing_token_completes_recovery(
  TSParser *self,
  StackVersion version,
  StackVersion first_candidate
) {
  if (!ts_stack_unconfirmed_recovery(self->stack, version)) return false;
  for (StackVersion v = first_candidate; v < ts_stack_version_count(self->stack); v++) {
    if (ts_stack_is_active(self->stack, v) &&
        ts_parser__state_finishes_root(self, ts_stack_state(self->stack, v))) return true;
  }
  return false;
}

static bool ts_parser__insert_missing_token(
  TSParser *self,
  StackVersion version,
  Subtree lookahead,
  Length position,
  bool require_reduction
) {
  TSStateId state = ts_stack_state(self->stack, version);
  TSSymbol first_symbol = require_reduction ? 1 : ts_parser__sole_missing_token(self, state);
  if (first_symbol == 0) return false;
  uint32_t symbol_limit = require_reduction ? self->language->token_count : first_symbol + 1;
  for (TSSymbol missing_symbol = first_symbol; missing_symbol < symbol_limit; missing_symbol++) {
    TSStateId state_after_missing_symbol = ts_table_cache_next_state(self->table_cache, self->language, state, missing_symbol);
    if (state_after_missing_symbol == 0 || state_after_missing_symbol == state) {
      continue;
    }

    TableEntry entry;
    ts_table_cache_entry(self->table_cache, self->language, state_after_missing_symbol, ts_subtree_symbol(lookahead), &entry);
    if (entry.action_count == 0) continue;
    bool has_reduce = ts_table_entry_has_reduce_action(&entry);
    if (has_reduce != require_reduction) continue;

    // In case the parser is currently outside of any included range, the lexer will
    // snap to the beginning of the next included range. The missing token's padding
    // must be assigned to position it within the next included range.
    ts_lexer_reset(&self->lexer, position);
    ts_lexer_mark_end(&self->lexer);
    Length padding = length_sub(self->lexer.token_end_position, position);

    StackVersion version_with_missing_tree = ts_stack_copy_version(self->stack, version);
    Subtree missing_tree = ts_subtree_new_missing_leaf(
      &self->tree_pool, missing_symbol, padding, self->language
    );
    ts_stack_push(
      self->stack, version_with_missing_tree,
      missing_tree, state_after_missing_symbol
    );
    ts_stack_set_unconfirmed_missing_token(self->stack, version_with_missing_tree, !require_reduction);

    if (ts_parser__do_all_potential_reductions(
      self, version_with_missing_tree,
      ts_subtree_symbol(lookahead)
    )) {
      LOG(
        "recover_with_missing symbol:%s, state:%u",
        SYM_NAME(missing_symbol),
        ts_stack_state(self->stack, version_with_missing_tree)
      );
      return true;
    }
    // The missing token did not help. Discard every version derived from it, including versions
    // that merged with one another, so that a failed attempt leaves no trace. Otherwise the zero-width
    // token survives, and recovery can insert it again and again at the same position.
    while (ts_stack_version_count(self->stack) > version_with_missing_tree) {
      ts_stack_remove_version(self->stack, version_with_missing_tree);
    }
  }
  return false;
}

static TSSymbol ts_parser__sole_missing_token(TSParser *self, TSStateId state) {
  TSSymbol result = 0;
  LookaheadIterator iter = ts_language_lookaheads(self->language, state);
  while (ts_lookahead_iterator__next(&iter)) {
    if (iter.symbol == 0 || iter.symbol >= self->language->token_count) continue;
    TSStateId next_state = ts_table_cache_next_state(self->table_cache, self->language, state, iter.symbol);
    if (next_state == 0 || next_state == state) continue;
    if (result != 0) return 0;
    result = iter.symbol;
  }
  if (result != 0 && ts_language_symbol_metadata(self->language, result).named) return 0;
  return result;
}

static bool ts_parser__check_progress(TSParser *self, Subtree *lookahead, const uint32_t *position, unsigned operations) {
  if (!ts_parser__check_lexer_progress(self, operations)) {
    if (lookahead && lookahead->ptr) {
      ts_subtree_release(&self->tree_pool, *lookahead);
    }
    return false;
  }
  if (!self->parse_options.progress_callback) return true;
  self->operation_count += operations;
  if (self->operation_count >= OP_COUNT_PER_PARSER_CALLBACK_CHECK) {
    self->operation_count = 0;
  }
  if (position != NULL) {
    self->parse_state.current_byte_offset = *position;
    self->parse_state.has_error = self->has_error;
  }
  if (
    self->operation_count == 0 &&
    self->parse_options.progress_callback(&self->parse_state)
  ) {
    if (lookahead && lookahead->ptr) {
      ts_subtree_release(&self->tree_pool, *lookahead);
    }
    return false;
  }
  return true;
}

static bool ts_parser__check_accept_progress(void *payload, unsigned operations) {
  TSParser *self = payload;
  uint32_t position = ts_stack_position(self->stack, self->accept_version).bytes;
  return ts_parser__check_progress(self, NULL, &position, operations);
}

static bool ts_parser__advance(
  TSParser *self,
  StackVersion version
) {
  TSStateId state = ts_stack_state(self->stack, version);
  uint32_t position = ts_stack_position(self->stack, version).bytes;
  uint32_t scanner_state_id = ts_stack_external_scanner_state_id(self->stack, version);

  TableEntry table_entry = {.action_count = 0};
  LookaheadToken lookahead = ts_parser__get_cached_token(
    self, version, state, position, scanner_state_id, &table_entry
  );

  bool needs_lex = !lookahead.subtree.ptr;
  for (;;) {
    if (needs_lex) {
      needs_lex = false;
      lookahead = ts_parser__lex(self, version, state);
      if (self->has_scanner_error || self->exceeded_max_operations) return false;

      if (lookahead.subtree.ptr) {
        ts_table_cache_entry(self->table_cache, self->language, state, ts_subtree_symbol(lookahead.subtree), &table_entry);
        bool deterministic_shift =
          ts_stack_version_count(self->stack) == 1 &&
          table_entry.action_count == 1 &&
          (table_entry.actions[0] & TS_PARSE_ACTION_TYPE_MASK) == TSParseActionTypeShift &&
          ts_subtree_total_bytes(lookahead.subtree) > 0;
        if (!deterministic_shift) {
          ts_parser__set_cached_token(
            self, position, scanner_state_id, lookahead, ts_stack_after_extra(self->stack, version)
          );
        }
      } else {
        // At the end of a non-terminal extra, the reduction is stored in the EOF table entry.
        ts_table_cache_entry(self->table_cache, self->language, state, ts_builtin_sym_end, &table_entry);
      }
    }

    if (!ts_parser__check_progress(self, &lookahead.subtree, &position, 1)) {
      return false;
    }

    bool did_reduce = false;
    StackVersion last_reduction_version = STACK_VERSION_NONE;
    for (uint32_t i = 0; i < table_entry.action_count; i++) {
      TSParseAction action = ts_language_parse_action(self->language, table_entry.actions[i]);

      switch (action.type) {
        case TSParseActionTypeShift: {
          TSStateId next_state;
          if (action.shift.extra) {
            next_state = state;
            LOG("shift_extra state:%u", state);
          } else {
            next_state = action.shift.state;
            LOG("shift state:%u, from_state:%u", next_state, state);
          }

          bool fork = i + 1 < table_entry.action_count;
          StackVersion shift_version = fork
            ? ts_stack_copy_version(self->stack, version)
            : version;

          if (fork) {
            ts_stack_record_ambiguous_extra(self->stack);
            ts_subtree_retain(lookahead.subtree);
          }
          ts_parser__shift(self, shift_version, next_state, lookahead.subtree, action.shift.extra);
          if (fork) break;
          return true;
        }

        case TSParseActionTypeReduce:
        case TSParseActionTypeReduceExtra: {
          bool is_fragile = table_entry.action_count > 1;
          bool is_extra = action.type == TSParseActionTypeReduceExtra;
          LOG("reduce sym:%s, child_count:%u, state:%u", SYM_NAME(action.reduce.symbol), action.reduce.child_count, state);
          StackVersion reduction_version = ts_parser__reduce(
            self, version, action.reduce.symbol, action.reduce.child_count,
            action.reduce.dynamic_precedence, action.reduce.production_id,
            is_fragile, is_extra
          );
          did_reduce = true;
          if (reduction_version != STACK_VERSION_NONE) {
            last_reduction_version = reduction_version;
          }
          break;
        }

        case TSParseActionTypeAccept: {
          LOG("accept state:%u", state);
          ts_parser__accept(self, version, lookahead.subtree);
          return true;
        }

        case TSParseActionTypeRecover: {
          ts_parser__recover(self, version, lookahead, false);
          return true;
        }
      }
    }

    if (last_reduction_version != STACK_VERSION_NONE) {
      ts_stack_renumber_version(self->stack, last_reduction_version, version);
      LOG_STACK();
      state = ts_stack_state(self->stack, version);

      // After completing a non-terminal extra rule (null lookahead), lex again from the new state.
      if (!lookahead.subtree.ptr) {
        needs_lex = true;
      } else {
        ts_table_cache_entry(self->table_cache, self->language, state, ts_subtree_symbol(lookahead.subtree), &table_entry);
      }

      continue;
    }

    if (did_reduce) {
      if (lookahead.subtree.ptr) {
        ts_subtree_release(&self->tree_pool, lookahead.subtree);
      }
      ts_stack_halt(self->stack, version);
      return true;
    }

    if (
      lookahead.is_keyword &&
      ts_subtree_symbol(lookahead.subtree) != self->language->keyword_capture_token &&
      !ts_language_is_reserved_word(self->language, state, ts_subtree_symbol(lookahead.subtree))
    ) {
      ts_table_cache_entry(self->table_cache, self->language, state, self->language->keyword_capture_token, &table_entry);
      if (table_entry.action_count > 0) {
        LOG(
          "switch from_keyword:%s, to_word_token:%s",
          TREE_NAME(lookahead.subtree),
          SYM_NAME(self->language->keyword_capture_token)
        );

        MutableSubtree mutable_lookahead = ts_subtree_make_mut(&self->tree_pool, lookahead.subtree);
        ts_subtree_set_symbol(&self->tree_pool, &mutable_lookahead, self->language->keyword_capture_token, self->language);
        lookahead.subtree = ts_subtree_from_mut(mutable_lookahead);
        continue;
      }
    }

    if (state == ERROR_STATE) {
      ts_parser__recover(self, version, lookahead, false);
      return true;
    }

    LOG("detect_error lookahead:%s", TREE_NAME(lookahead.subtree));
    ts_stack_pause(self->stack, version, lookahead);
    return true;
  }
}

static unsigned ts_parser__condense_stack(TSParser *self) {
  if (
    ts_stack_version_count(self->stack) == 1 &&
    ts_stack_is_active(self->stack, 0) &&
    !ts_stack_unconfirmed_recovery(self->stack, 0)
  ) {
    return ts_stack_state(self->stack, 0) == ERROR_STATE
      ? UINT_MAX
      : ts_stack_error_cost(self->stack, 0);
  }

  bool made_changes = false;
  unsigned min_error_cost = UINT_MAX;
  for (StackVersion i = 0; i < ts_stack_version_count(self->stack); i++) {
    // Prune any versions that have been marked for removal.
    if (ts_stack_is_halted(self->stack, i)) {
      ts_stack_remove_version(self->stack, i);
      i--;
      continue;
    }

    if (ts_stack_unconfirmed_recovery(self->stack, i) &&
        ts_stack_node_count_since_recovery(self->stack, i) > MAX_PROVISIONAL_RECOVERY_NODE_COUNT) {
      ts_stack_set_unconfirmed_recovery(self->stack, i, false);
    }

    // Keep track of the minimum error cost of any stack version so
    // that it can be returned.
    ErrorStatus status_i = ts_parser__version_status(self, i);
    if (!status_i.is_in_error && status_i.cost < min_error_cost) {
      min_error_cost = status_i.cost;
    }

    // Merge this version into an earlier one in the same state before comparing
    // it with the others. Otherwise, whether it survives would depend on the
    // versions compared first, since merging changes their node counts. After
    // this, no pair compared below can be merged.
    bool merged = false;
    for (StackVersion j = 0; j < i; j++) {
      if (ts_stack_merge(self->stack, j, i)) {
        merged = true;
        break;
      }
    }
    if (merged) {
      made_changes = true;
      i--;
      continue;
    }

    // Examine each pair of stack versions, removing any versions that
    // are clearly worse than another version. Ensure that the versions
    // are ordered from most promising to least promising.
    for (StackVersion j = 0; j < i; j++) {
      ErrorStatus status_j = ts_parser__version_status(self, j);

      switch (ts_parser__compare_ranked_versions(self, status_j, status_i)) {
        case ErrorComparisonTakeLeft:
          made_changes = true;
          ts_stack_remove_version(self->stack, i);
          i--;
          j = i;
          break;

        case ErrorComparisonPreferLeft:
        case ErrorComparisonNone:
          break;

        case ErrorComparisonPreferRight:
          made_changes = true;
          ts_stack_swap_versions(self->stack, i, j);
          status_i = ts_parser__version_status(self, i);
          break;

        case ErrorComparisonTakeRight:
          made_changes = true;
          ts_stack_remove_version(self->stack, j);
          i--;
          j--;
          break;
      }
    }
  }

  // Enforce a hard upper bound on the number of stack versions by
  // discarding the least promising versions.
  while (ts_stack_version_count(self->stack) > MAX_VERSION_COUNT) {
    ts_parser__log_version_limit(self, "condense", MAX_VERSION_COUNT, true);
    ts_stack_remove_version(self->stack, MAX_VERSION_COUNT);
    made_changes = true;
  }

  // While any version is active, resume at most one paused version, and only if its recovery is
  // still provisional or its penalized cost beats every active version; remove the others.
  // Resuming every paused version absorbs preceding valid declarations into ERROR nodes, and
  // resuming none loses declarations that only a paused path keeps.
  if (ts_stack_version_count(self->stack) > 0) {
    unsigned active_cost = UINT_MAX;
    bool has_active_version = false;
    bool resumed_paused_version = false;
    for (StackVersion i = 0, n = ts_stack_version_count(self->stack); i < n; i++) {
      if (!ts_stack_is_paused(self->stack, i)) {
        unsigned cost = ts_stack_error_cost(self->stack, i);
        if (cost < active_cost) active_cost = cost;
        has_active_version = true;
        continue;
      }
      bool can_resume_recovery = !resumed_paused_version &&
        (ts_stack_unconfirmed_recovery(self->stack, i) ||
         ts_parser__version_status(self, i).cost < active_cost);
      if ((has_active_version && !can_resume_recovery) || self->accept_count >= MAX_VERSION_COUNT) {
        ts_parser__log_version_limit(self, "accept", i, self->accept_count >= MAX_VERSION_COUNT);
        ts_stack_remove_version(self->stack, i);
        made_changes = true;
        i--;
        n--;
        continue;
      }
      LOG("resume version:%u", i);
      unsigned cost = ts_stack_error_cost(self->stack, i);
      if (cost < min_error_cost) min_error_cost = cost;
      LookaheadToken lookahead = ts_stack_resume(self->stack, i);
      ts_parser__handle_error(self, i, lookahead);
      if (self->accept_version != STACK_VERSION_NONE) {
        for (StackVersion j = i + 1; j < n; j++) {
          if (ts_stack_is_paused(self->stack, j)) {
            ts_stack_remove_version(self->stack, j--);
            n--;
          }
        }
        return min_error_cost;
      }
      resumed_paused_version = true;
      has_active_version = true;
    }
  }

  if (made_changes) {
    LOG("condense");
    LOG_STACK();
  }

  return min_error_cost;
}

static bool ts_parser__compress_subtree(TSParser *self, MutableSubtree tree) {
  if (ts_subtree_repeat_depth(ts_subtree_from_mut(tree)) == 0) return true;
  Subtree child1 = ts_subtree_children(tree)[0];
  Subtree child2 = ts_subtree_children(tree)[tree.ptr->child_count - 1];
  long repeat_delta = (long)ts_subtree_repeat_depth(child1) - (long)ts_subtree_repeat_depth(child2);
  if (repeat_delta <= 0) return true;

  unsigned n = (unsigned)repeat_delta;
  for (unsigned i = n / 2; i > 0; i /= 2) {
    ts_subtree_compress(tree, i, self->language, &self->tree_pool.tree_stack);
    n -= i;

    // We scale the operation count increment in `ts_parser__check_progress` proportionately to the compression
    // size since larger values of i take longer to process. Shifting by 4 empirically provides good check
    // intervals (e.g. 193 operations when i=3100) to prevent blocking during large compressions.
    unsigned operations = i >> 4 > 0 ? i >> 4 : 1;
    if (!ts_parser__check_progress(self, NULL, NULL, operations)) {
      return false;
    }
  }
  return true;
}

static inline bool ts_parser__needs_balancing(Subtree tree) {
  return
    !ts_subtree_is_inline(tree) &&
    ts_subtree_heap_flag(tree.ptr, SUBTREE_FLAG_UNBALANCED_REPEAT) &&
    ts_subtree_heap_ref_count(tree.ptr) == 1;
}

static bool ts_parser__balance_subtree(TSParser *self) {
  Subtree finished_tree = self->finished_tree;

  // If we haven't canceled balancing in progress before, then we want to clear the tree stack and
  // push the initial finished tree onto it. Otherwise, if we're resuming balancing after a
  // cancellation, we don't want to clear the tree stack.
  if (!self->canceled_balancing) {
    array_clear(&self->tree_pool.tree_stack);
    if (ts_parser__needs_balancing(finished_tree)) {
      array_push(&self->tree_pool.tree_stack, ts_subtree_to_mut_unsafe(finished_tree));
    }
  }

  while (self->tree_pool.tree_stack.size > 0) {
    if (!ts_parser__check_progress(self, NULL, NULL, 1)) {
      return false;
    }

    MutableSubtree tree = *array_get(&self->tree_pool.tree_stack,
      self->tree_pool.tree_stack.size - 1
    );

    if (!ts_parser__compress_subtree(self, tree)) {
      return false;
    }

    (void)array_pop(&self->tree_pool.tree_stack);

    for (uint32_t i = 0; i < tree.ptr->child_count; i++) {
      Subtree child = ts_subtree_children(tree)[i];
      if (ts_parser__needs_balancing(child)) {
        array_push(&self->tree_pool.tree_stack, ts_subtree_to_mut_unsafe(child));
      }
    }
  }

  return true;
}

static bool ts_parser_has_outstanding_parse(TSParser *self) {
  return (
    self->accept_version != STACK_VERSION_NONE ||
    self->canceled_balancing ||
    self->external_scanner_payload ||
    ts_stack_state(self->stack, 0) != 1 ||
    ts_stack_node_count_since_error(self->stack, 0) != 0
  );
}

// Parser - Public

static void scanner_state_table_reset(ScannerStateTable *self);

TSParser *ts_parser_new(void) {
  TSParser *self = ts_calloc(1, sizeof(TSParser));
  ts_lexer_init(&self->lexer);
  ts_lexer_set_progress_callback(&self->lexer, ts_parser__check_lexer_progress, self);
  array_init(&self->reduce_actions);
  array_reserve(&self->reduce_actions, 4);
  self->tree_pool = ts_subtree_pool_new(32);
  self->stack = ts_stack_new(&self->tree_pool);
  self->scanner_states.seed = scanner_state_mix64(
    (uint64_t)(uintptr_t)self ^ UINT64_C(0x736f6d6570736575)
  );
  self->finished_tree = NULL_SUBTREE;
  self->dot_graph_file = NULL;
  self->language = NULL;
  self->has_scanner_error = false;
  self->has_error = false;
  self->canceled_balancing = false;
  self->external_scanner_payload = NULL;
  self->operation_count = 0;
  self->accept_version = STACK_VERSION_NONE;
  self->token_cache = (TokenCache) {.token = NULL_LOOKAHEAD};
  return self;
}

void ts_parser_delete(TSParser *self) {
  if (!self) return;

  ts_parser_set_language(self, NULL);
  ts_stack_delete(self->stack);
  if (self->reduce_actions.contents) {
    array_delete(&self->reduce_actions);
  }
  ts_wasm_store_delete(self->wasm_store);
  ts_lexer_delete(&self->lexer);
  ts_parser__clear_cached_token(self);
  ts_subtree_pool_delete(&self->tree_pool);
  array_delete(&self->scanner_states.bytes);
  array_delete(&self->scanner_states.entries);
  array_delete(&self->scanner_states.buckets);
  array_delete(&self->trailing_extras);
  array_delete(&self->trailing_extras2);
  array_delete(&self->scratch_trees);
  ts_free(self->table_cache);
  ts_free(self);
}

void ts_parser_abandon_after_alloc_failure(TSParser *self) {
  if (!self) return;
  ts_parser__external_scanner_destroy(self);
  ts_language_delete(self->language);
}

void ts_parser_set_max_operations(TSParser *self, uint64_t max_operations) {
  self->max_operations = max_operations;
}

uint64_t ts_parser_last_parse_operations(const TSParser *self) {
  return self ? self->total_operation_count + self->lexer.advance_count / 64u : 0;
}

const TSLanguage *ts_parser_language(const TSParser *self) {
  return self->language;
}

bool ts_parser_set_language(TSParser *self, const TSLanguage *language) {
  ts_parser_reset(self);
  ts_language_delete(self->language);
  self->language = NULL;
  if (self->table_cache) ts_table_cache_invalidate(self->table_cache);
  ts_lexer_clear_cache(&self->lexer);

  if (language) {
    if (!ts_language_has_supported_abi(language)) return false;

    if (!ts_language_is_parseable(language)) return false;

    if (ts_language_is_wasm(language)) {
      if (
        !self->wasm_store ||
        !ts_wasm_store_start(self->wasm_store, &self->lexer.data, language)
      ) return false;
    }

  }

  self->language = ts_language_copy(language);
  return true;
}

// Allocate the table cache now if the parser has a language and no cache yet, and return the newly allocated
// cache, or NULL if none was allocated. `ts_parser_parse` does this itself; the Wasm binding calls it at the start
// of a memory-limited parse to detach the new cache from that parse's budget before lending it.
void *ts_parser_reserve_table_cache(TSParser *self) {
  if (self->table_cache || !self->language) return NULL;
  self->table_cache = ts_calloc(1, sizeof(TableCache));
  ts_table_cache_invalidate(self->table_cache);
  return self->table_cache;
}

// Exchange the table caches of two parsers, so that a parser created for one parse can use another parser's
// cache instead of allocating its own. Either parser's language may change while it holds the other's cache,
// so both caches are invalidated.
void ts_parser_swap_table_cache(TSParser *self, TSParser *other) {
  TableCache *table_cache = self->table_cache;
  self->table_cache = other->table_cache;
  other->table_cache = table_cache;
  if (self->table_cache) ts_table_cache_invalidate(self->table_cache);
  if (other->table_cache) ts_table_cache_invalidate(other->table_cache);
}

TSLogger ts_parser_logger(const TSParser *self) {
  return self->lexer.logger;
}

void ts_parser_set_logger(TSParser *self, TSLogger logger) {
  ts_lexer_set_logger(&self->lexer, logger);
}

#ifdef __wasm__
void ts_parser_print_dot_graphs(TSParser *self, int fd) {
  (void)self;
  (void)fd;
}
#else
void ts_parser_print_dot_graphs(TSParser *self, int fd) {
  if (self->dot_graph_file) {
    fclose(self->dot_graph_file);
  }

  if (fd >= 0) {
    #ifdef _WIN32
    self->dot_graph_file = _fdopen(fd, "a");
    #else
    self->dot_graph_file = fdopen(fd, "a");
    #endif
  } else {
    self->dot_graph_file = NULL;
  }
}
#endif

bool ts_parser_set_included_ranges(
  TSParser *self,
  const TSRange *ranges,
  uint32_t count
) {
  return ts_lexer_set_included_ranges(&self->lexer, ranges, count);
}

const TSRange *ts_parser_included_ranges(const TSParser *self, uint32_t *count) {
  return ts_lexer_included_ranges(&self->lexer, count);
}

void ts_parser_reset(TSParser *self) {
  ts_parser__external_scanner_destroy(self);
  if (self->wasm_store) {
    ts_wasm_store_reset(self->wasm_store);
  }

  ts_lexer_reset(&self->lexer, length_zero());
  ts_stack_clear(self->stack);
  ts_parser__clear_cached_token(self);
  if (self->finished_tree.ptr) {
    ts_subtree_release(&self->tree_pool, self->finished_tree);
    self->finished_tree = NULL_SUBTREE;
  }
  ts_subtree_pool_clear(&self->tree_pool);
  scanner_state_table_reset(&self->scanner_states);
  self->accept_count = 0;
  self->accept_version = STACK_VERSION_NONE;
  self->accept_slices = (StackSliceArray) array_new();
  self->accept_from_condense = false;
  self->has_scanner_error = false;
  self->has_error = false;
  self->canceled_balancing = false;
  self->exceeded_max_operations = false;
  self->max_operations = 0;
  self->lexer.advance_count = 0;
  self->parse_options = (TSParseOptions) {0};
  self->parse_state = (TSParseState) {0};
}

TSTree *ts_parser_parse(
  TSParser *self,
  TSInput input
) {
  TSTree *result = NULL;
  self->total_operation_count = 0;
  self->exceeded_max_operations = false;
  self->lexer.advance_count = 0;
  self->lexer.has_operation_limit = self->max_operations != 0;
  if (!self->language || !input.read) {
    self->max_operations = 0;
    return NULL;
  }
  ts_parser_reserve_table_cache(self);

  if (ts_language_is_wasm(self->language)) {
    if (!self->wasm_store) {
      self->max_operations = 0;
      return NULL;
    }
    ts_wasm_store_start(self->wasm_store, &self->lexer.data, self->language);
  }

  ts_lexer_set_input(&self->lexer, input);
  self->operation_count = 0;

  if (ts_parser_has_outstanding_parse(self)) {
    LOG("resume_parsing");
    if (self->canceled_balancing) goto balance;
  } else {
    ts_parser__external_scanner_create(self);
    if (self->has_scanner_error) goto exit;
    LOG("new_parse");
  }

  if (self->accept_version != STACK_VERSION_NONE) {
    if (!ts_parser__finish_accept(self)) {
      if (self->exceeded_max_operations) goto exit;
      return NULL;
    }
    if (self->accept_from_condense) {
      self->accept_from_condense = false;
      if (self->finished_tree.ptr && ts_subtree_error_cost(self->finished_tree) < self->accept_min_error_cost) {
        ts_stack_clear(self->stack);
        goto balance;
      }
    }
  }

  uint32_t position = 0, last_position = 0, version_count = 0;
  do {
    for (
      StackVersion version = 0;
      version_count = ts_stack_version_count(self->stack),
      version < version_count;
      version++
    ) {
      while (ts_stack_is_active(self->stack, version)) {
        LOG(
          "process version:%u, version_count:%u, state:%d, row:%u, col:%u",
          version,
          ts_stack_version_count(self->stack),
          ts_stack_state(self->stack, version),
          ts_stack_position(self->stack, version).extent.row,
          ts_stack_position(self->stack, version).extent.column
        );

        if (!ts_parser__advance(self, version)) {
          if (self->has_scanner_error || self->exceeded_max_operations) goto exit;
          return NULL;
        }
        if (self->accept_version != STACK_VERSION_NONE && !ts_parser__finish_accept(self)) {
          if (self->exceeded_max_operations) goto exit;
          return NULL;
        }

        LOG_STACK();

        position = ts_stack_position(self->stack, version).bytes;
        if (position > last_position || (version > 0 && position == last_position)) {
          last_position = position;
          break;
        }
      }
    }

    unsigned min_error_cost = ts_parser__condense_stack(self);
    if (self->accept_version != STACK_VERSION_NONE) {
      self->accept_from_condense = true;
      self->accept_min_error_cost = min_error_cost;
      if (!ts_parser__finish_accept(self)) {
        if (self->exceeded_max_operations) goto exit;
        return NULL;
      }
      self->accept_from_condense = false;
    }

    // Clear stack references to the finished tree before balancing its subtrees in place.
    if (self->finished_tree.ptr && ts_subtree_error_cost(self->finished_tree) < min_error_cost) {
      ts_stack_clear(self->stack);
      break;
    }
  } while (version_count != 0);

balance:
  ts_assert(self->finished_tree.ptr);
  if (!ts_parser__balance_subtree(self)) {
    if (self->exceeded_max_operations) goto exit;
    self->canceled_balancing = true;
    return NULL;
  }
  self->canceled_balancing = false;
  LOG("done");
  LOG_TREE(self->finished_tree);

  ts_stack_clear(self->stack);
  ts_parser__clear_cached_token(self);
  TreeArenaSlab *arena = ts_subtree_pool_take_arena(&self->tree_pool);
  result = ts_tree_new(
    self->finished_tree,
    self->language,
    self->lexer.included_ranges,
    self->lexer.included_range_count,
    arena
  );
  self->finished_tree = NULL_SUBTREE;

exit:
  ts_lexer_flush_operations(&self->lexer);
  ts_parser_reset(self);
  return result;
}

TSTree *ts_parser_parse_with_options(
  TSParser *self,
  TSInput input,
  TSParseOptions parse_options
) {
  self->parse_options = parse_options;
  self->parse_state.payload = parse_options.payload;
  TSTree *result = ts_parser_parse(self, input);
  self->max_operations = 0;
  self->parse_options = (TSParseOptions) {0};
  return result;
}

TSTree *ts_parser_parse_string(
  TSParser *self,
  const char *string,
  uint32_t length
) {
  return ts_parser_parse_string_encoding(self, string, length, TSInputEncodingUTF8);
}

TSTree *ts_parser_parse_string_encoding(
  TSParser *self,
  const char *string,
  uint32_t length,
  TSInputEncoding encoding
) {
  TSStringInput input = {string, length};
  return ts_parser_parse(self, (TSInput) {
    &input,
    ts_string_input_read,
    encoding,
    NULL,
  });
}

void ts_parser_set_wasm_store(TSParser *self, TSWasmStore *store) {
  if (self->language && ts_language_is_wasm(self->language)) {
    // Copy the assigned language into the new store.
    const TSLanguage *copy = ts_language_copy(self->language);
    ts_parser_set_language(self, copy);
    ts_language_delete(copy);
  }

  ts_wasm_store_delete(self->wasm_store);
  self->wasm_store = store;
}

TSWasmStore *ts_parser_take_wasm_store(TSParser *self) {
  if (self->language && ts_language_is_wasm(self->language)) {
    ts_parser_set_language(self, NULL);
  }

  TSWasmStore *result = self->wasm_store;
  self->wasm_store = NULL;
  return result;
}

static void scanner_state_table_reset(ScannerStateTable *self) {
  if (self->entries.size == 0 && self->buckets.size == 0) return;
  if (self->buckets.capacity > 2048 || self->bytes.capacity > 65536) {
    array_delete(&self->bytes);
    array_delete(&self->entries);
    array_delete(&self->buckets);
  } else {
    array_clear(&self->bytes);
    array_clear(&self->entries);
    array_clear(&self->buckets);
  }
  self->seed = scanner_state_mix64(self->seed + UINT64_C(0x9e3779b97f4a7c15));
}

static uint64_t scanner_state_mix64(uint64_t x) {
  x = (x ^ (x >> 30)) * UINT64_C(0xbf58476d1ce4e5b9);
  x = (x ^ (x >> 27)) * UINT64_C(0x94d049bb133111eb);
  return x ^ (x >> 31);
}

static inline uint64_t scanner_state_load_le64(const uint8_t *p) {
  return (uint64_t)p[0] |
    ((uint64_t)p[1] << 8) |
    ((uint64_t)p[2] << 16) |
    ((uint64_t)p[3] << 24) |
    ((uint64_t)p[4] << 32) |
    ((uint64_t)p[5] << 40) |
    ((uint64_t)p[6] << 48) |
    ((uint64_t)p[7] << 56);
}

#undef LOG
