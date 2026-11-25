#ifndef TREE_SITTER_TABLE_CACHE_H_
#define TREE_SITTER_TABLE_CACHE_H_

#ifdef __cplusplus
extern "C" {
#endif

#include <stdint.h>
#include <string.h>
#include "./language.h"

#define TABLE_CACHE_BITS 11

// A direct-mapped memo of compact parse-table lookups. Each slot holds
// `epoch << 48 | state << 32 | symbol << 16 | value` and matches only while `epoch` is current, so a
// zero-initialized cache is empty and invalidating it rewrites no slot until the epoch wraps.
typedef struct {
  uint64_t slots[1u << TABLE_CACHE_BITS];
  uint16_t epoch;
} TableCache;

// Called whenever the language changes, so that no slot stored for another language matches.
static inline void ts_table_cache_invalidate(TableCache *self) {
  if (++self->epoch == 0) {
    memset(self->slots, 0, sizeof(self->slots));
    self->epoch = 1;
  }
}

// Terminal cells and nonterminal gotos share slots because terminal and nonterminal symbols are disjoint.
static inline uint16_t ts_table_cache_lookup(
  TableCache *self,
  const TSLanguage *language,
  TSStateId state,
  TSSymbol symbol,
  bool is_goto
) {
  if (!is_goto && state < language->large_state_count) {
    return language->parse_table[(uint32_t)state * language->token_count + symbol];
  }
  uint32_t key = (uint32_t)state << 16 | symbol;
  uint64_t *slot = &self->slots[(key * 0x9E3779B1u) >> (32 - TABLE_CACHE_BITS)];
  uint64_t tagged = (uint64_t)self->epoch << 48 | (uint64_t)key << 16;
  if ((*slot ^ tagged) <= UINT16_MAX) return (uint16_t)*slot;
  uint16_t value = is_goto
    ? ts_language_lookup_goto(language, state, symbol)
    : ts_language_lookup(language, state, symbol);
  *slot = tagged | value;
  return value;
}

static inline void ts_table_cache_entry(
  TableCache *self,
  const TSLanguage *language,
  TSStateId state,
  TSSymbol symbol,
  TableEntry *result
) {
  if (symbol >= language->token_count) {
    *result = (TableEntry) {0};
    return;
  }
  uint16_t cell = ts_table_cache_lookup(self, language, state, symbol, false);
  const uint32_t *actions = &language->parse_actions[cell & TS_PARSE_TABLE_ACTION_MASK];
  result->action_count = *actions >> TS_PARSE_ACTION_COUNT_SHIFT;
  result->is_reusable = (cell & TS_PARSE_TABLE_REUSABLE_FLAG) != 0;
  result->actions = actions;
}

static inline bool ts_table_cache_has_actions(
  TableCache *self,
  const TSLanguage *language,
  TSStateId state,
  TSSymbol symbol
) {
  return symbol < language->token_count && ts_table_cache_lookup(self, language, state, symbol, false) != 0;
}

static inline TSStateId ts_table_cache_next_state(
  TableCache *self,
  const TSLanguage *language,
  TSStateId state,
  TSSymbol symbol
) {
  if (symbol >= language->symbol_count || state >= language->state_count) return 0;
  if (symbol >= language->token_count) return ts_table_cache_lookup(self, language, state, symbol, true);
  TableEntry entry;
  ts_table_cache_entry(self, language, state, symbol, &entry);
  if (entry.action_count == 0) return 0;
  TSParseAction action = ts_language_parse_action(language, entry.actions[entry.action_count - 1]);
  if (action.type != TSParseActionTypeShift) return 0;
  return action.shift.extra ? state : action.shift.state;
}

#ifdef __cplusplus
}
#endif

#endif  // TREE_SITTER_TABLE_CACHE_H_
