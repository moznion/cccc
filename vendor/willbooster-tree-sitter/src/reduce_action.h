#ifndef TREE_SITTER_REDUCE_ACTION_H_
#define TREE_SITTER_REDUCE_ACTION_H_

#ifdef __cplusplus
extern "C" {
#endif

#include "./array.h"
#include "tree_sitter/api.h"

typedef struct {
  uint32_t count;
  TSSymbol symbol;
  int dynamic_precedence;
  unsigned short production_id;
  bool is_extra;
} ReduceAction;

typedef Array(ReduceAction) ReduceActionSet;

// Orders reductions by descending symbol, then by their remaining fields, so that sorting a set gives the same
// sequence whatever order its reductions were added in.
static inline bool ts_reduce_action_precedes(ReduceAction left, ReduceAction right) {
  if (left.symbol != right.symbol) return left.symbol > right.symbol;
  if (left.count != right.count) return left.count < right.count;
  if (left.is_extra != right.is_extra) return !left.is_extra;
  if (left.production_id != right.production_id) return left.production_id < right.production_id;
  return left.dynamic_precedence < right.dynamic_precedence;
}

// Reductions of the same symbol, child count, and extra flag are duplicates; the set keeps the one that
// `ts_reduce_action_precedes` orders first, so its contents do not depend on the order of additions.
static inline void ts_reduce_action_set_add(ReduceActionSet *self,
                                            ReduceAction new_action) {
  for (uint32_t i = 0; i < self->size; i++) {
    ReduceAction action = self->contents[i];
    if (action.symbol == new_action.symbol && action.count == new_action.count &&
        action.is_extra == new_action.is_extra) {
      if (ts_reduce_action_precedes(new_action, action)) self->contents[i] = new_action;
      return;
    }
  }
  array_push(self, new_action);
}

#ifdef __cplusplus
}
#endif

#endif  // TREE_SITTER_REDUCE_ACTION_H_
