#ifndef TREE_SITTER_TREE_H_
#define TREE_SITTER_TREE_H_

#include "./subtree.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
  const Subtree *child;
  const Subtree *parent;
  Length position;
  TSSymbol alias_symbol;
} ParentCacheEntry;

struct TSTree {
  Subtree root;
  TreeArenaSlab *arena;
  const TSLanguage *language;
#ifdef __wasm__
  uint32_t language_context_id;
  const TSLanguage *volatile unparseable_language;
#endif
  TSRange *included_ranges;
  unsigned included_range_count;
};

TSTree *ts_tree_new(
  Subtree root,
  const TSLanguage *language,
  const TSRange *included_ranges,
  unsigned included_range_count,
  TreeArenaSlab *arena
);
TSNode ts_node_new(const TSTree *tree, const Subtree *subtree, Length position, TSSymbol alias);
char *ts_node_string_pretty(TSNode self, size_t initial_indent_level);

#ifdef __cplusplus
}
#endif

#endif  // TREE_SITTER_TREE_H_
