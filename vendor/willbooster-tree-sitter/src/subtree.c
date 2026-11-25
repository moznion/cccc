// code-gauge-allow-file fileNcss -- subtree arena allocation, summaries, and formatting share private helpers here
#include <ctype.h>
#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>
#include <string.h>
#include <stdio.h>
#include "./alloc.h"
#include "./array.h"
#include "./atomic.h"
#include "./subtree.h"
#include "./length.h"
#include "./language.h"
#include "./error_costs.h"
#include "./ts_assert.h"

// Each slab, header included, is exactly one 4 KiB malloc request. Observed with the default
// allocator configuration of macOS 27 on arm64 (it differs, e.g., under MallocLargeCache=0):
// freed blocks of 8 KiB or more stayed resident, so with 16-64 KiB slabs the peak footprint of
// a caller that parses, walks, and drops trees in a loop grew with every cycle, while freeing
// many blocks of at most 4 KiB let the allocator return the memory. A request just above a size
// class (e.g., 4096 + 32 bytes) would also be rounded up to the next class.
#define TREE_ARENA_SLAB_BYTES 4096u
#define TREE_ARENA_SLAB_WORDS ((TREE_ARENA_SLAB_BYTES - sizeof(TreeArenaSlab)) / sizeof(uint64_t))
#define SUBTREE_HEADER_WORDS (sizeof(SubtreeHeapData) / sizeof(uint64_t))

_Static_assert(sizeof(Subtree) == 8, "an inline subtree must fit in 8 bytes");
_Static_assert(sizeof(SubtreeHeapData) == 56, "a heap subtree must fit in 56 bytes");

// SubtreeArray

void ts_subtree_array_copy(SubtreeArray self, SubtreeArray *dest) {
  dest->size = self.size;
  dest->capacity = self.capacity;
  dest->contents = self.contents;
  if (self.capacity > 0) {
    dest->contents = ts_calloc(self.capacity, sizeof(Subtree));
    memcpy(dest->contents, self.contents, self.size * sizeof(Subtree));
    for (uint32_t i = 0; i < self.size; i++) {
      ts_subtree_retain(*array_get(dest, i));
    }
  }
}

void ts_subtree_array_clear(SubtreePool *pool, SubtreeArray *self) {
  for (uint32_t i = 0; i < self->size; i++) {
    ts_subtree_release(pool, *array_get(self, i));
  }
  array_clear(self);
}

void ts_subtree_array_delete(SubtreePool *pool, SubtreeArray *self) {
  ts_subtree_array_clear(pool, self);
  array_delete(self);
}

void ts_subtree_array_remove_trailing_extras(
  SubtreeArray *self,
  SubtreeArray *destination
) {
  array_clear(destination);
  while (self->size > 0) {
    Subtree last = *array_get(self, self->size - 1);
    if (ts_subtree_extra(last)) {
      self->size--;
      array_push(destination, last);
    } else {
      break;
    }
  }
  ts_subtree_array_reverse(destination);
}

void ts_subtree_array_reverse(SubtreeArray *self) {
  for (uint32_t i = 0, limit = self->size / 2; i < limit; i++) {
    size_t reverse_index = self->size - 1 - i;
    Subtree swap = *array_get(self, i);
    *array_get(self, i) = *array_get(self, reverse_index);
    *array_get(self, reverse_index) = swap;
  }
}

// TreeArena & SubtreePool

void ts_tree_arena_slab_free_all(TreeArenaSlab *head) {
  if (!head) return;
  TreeArenaLargeBlock *large = head->large_blocks;
  while (large) {
    TreeArenaLargeBlock *next_large = large->next;
    ts_free(large);
    large = next_large;
  }
  TreeArenaSlab *slab = head;
  while (slab) {
    TreeArenaSlab *next_slab = slab->next;
    ts_free(slab);
    slab = next_slab;
  }
}

static void ts_tree_arena_push_slab(TreeArena *arena) {
  TreeArenaSlab *slab = ts_malloc(TREE_ARENA_SLAB_BYTES);
  slab->next = arena->head;
  slab->large_blocks = arena->head ? arena->head->large_blocks : NULL;
  if (arena->head) arena->head->large_blocks = NULL;
  slab->ref_count = 1;
  slab->used = 0;
  arena->head = slab;
}

SubtreePool ts_subtree_pool_new(uint32_t capacity) {
  (void)capacity;
  return (SubtreePool) {
    .arena = {0},
    .tree_stack = array_new(),
  };
}

void ts_subtree_pool_clear(SubtreePool *self) {
  ts_tree_arena_slab_free_all(self->arena.head);
  self->arena = (TreeArena) {0};
  array_clear(&self->tree_stack);
}

void ts_subtree_pool_delete(SubtreePool *self) {
  ts_tree_arena_slab_free_all(self->arena.head);
  self->arena = (TreeArena) {0};
  if (self->tree_stack.contents) array_delete(&self->tree_stack);
}

TreeArenaSlab *ts_subtree_pool_take_arena(SubtreePool *self) {
  TreeArenaSlab *head = self->arena.head;
  self->arena = (TreeArena) {0};
  return head;
}

SubtreeHeapData *ts_subtree_pool_alloc_node(SubtreePool *self, uint32_t child_count) {
  TreeArena *arena = &self->arena;
  size_t words = (size_t)child_count + SUBTREE_HEADER_WORDS;
  uint64_t *slot;
  if (child_count < TREE_ARENA_BIN_COUNT && arena->free_bins[child_count]) {
    slot = arena->free_bins[child_count];
    arena->free_bins[child_count] = *(void **)slot;
  } else if (words > TREE_ARENA_SLAB_WORDS) {
    if (!arena->head) ts_tree_arena_push_slab(arena);
    TreeArenaLargeBlock *block = ts_malloc(sizeof(TreeArenaLargeBlock) + words * sizeof(uint64_t));
    block->prev = NULL;
    block->next = arena->head->large_blocks;
    if (block->next) block->next->prev = block;
    arena->head->large_blocks = block;
    slot = block->data;
  } else {
    if (!arena->head || arena->head->used + words > TREE_ARENA_SLAB_WORDS) {
      ts_tree_arena_push_slab(arena);
    }
    slot = arena->head->data + arena->head->used;
    arena->head->used += (uint32_t)words;
  }
  SubtreeHeapData *data = (SubtreeHeapData *)(slot + child_count);
  data->ref_count = 1;
  data->flags = 0;
  return data;
}

static void ts_subtree_pool_free_node(SubtreePool *self, SubtreeHeapData *data) {
  uint32_t child_count = data->child_count;
  void *start = (Subtree *)data - child_count;
  if ((size_t)child_count + SUBTREE_HEADER_WORDS > TREE_ARENA_SLAB_WORDS) {
    TreeArenaLargeBlock *block = (TreeArenaLargeBlock *)((char *)start - offsetof(TreeArenaLargeBlock, data));
    if (block->prev) {
      block->prev->next = block->next;
    } else if (self->arena.head) {
      self->arena.head->large_blocks = block->next;
    }
    if (block->next) block->next->prev = block->prev;
    ts_free(block);
  } else if (child_count < TREE_ARENA_BIN_COUNT) {
    *(void **)start = self->arena.free_bins[child_count];
    self->arena.free_bins[child_count] = start;
  }
}

// Subtree

static inline bool ts_subtree_can_inline(Length padding, Length size) {
  return
    padding.bytes < TS_MAX_INLINE_TREE_LENGTH &&
    padding.extent.row < TS_MAX_INLINE_TREE_ROWS &&
    padding.extent.column < TS_MAX_INLINE_TREE_LENGTH &&
    size.bytes < TS_MAX_INLINE_TREE_LENGTH &&
    size.extent.row == 0 &&
    size.extent.column == size.bytes;
}

static inline Subtree ts_subtree_pack_inline(
  TSSymbol symbol,
  Length padding,
  Length size,
  TSSymbolMetadata metadata,
  uint64_t inline_flags
) {
  uint64_t raw =
    SUBTREE_INLINE_BIT |
    inline_flags |
    (metadata.visible ? SUBTREE_INLINE_VISIBLE : 0ULL) |
    (metadata.named ? SUBTREE_INLINE_NAMED : 0ULL) |
    ((uint64_t)symbol << SUBTREE_INLINE_SYMBOL_SHIFT) |
    ((uint64_t)size.bytes << SUBTREE_INLINE_SIZE_SHIFT) |
    ((uint64_t)padding.bytes << SUBTREE_INLINE_PAD_BYTES_SHIFT) |
    ((uint64_t)padding.extent.row << SUBTREE_INLINE_PAD_ROWS_SHIFT) |
    ((uint64_t)padding.extent.column << SUBTREE_INLINE_PAD_COLS_SHIFT);
  return (Subtree) {.bits = ts_subtree_inline_raw(raw)};
}

// code-gauge-allow functionParameterCount -- leaf construction receives symbol, spans, and scanner state directly
Subtree ts_subtree_new_leaf(
  SubtreePool *pool, TSSymbol symbol, Length padding, Length size,
  bool has_external_tokens, uint32_t scanner_state_id, const TSLanguage *language
) {
  TSSymbolMetadata metadata = ts_language_symbol_metadata(language, symbol);
  bool extra = symbol == ts_builtin_sym_end;

  if (
    symbol <= TS_MAX_INLINE_TREE_SYMBOL &&
    scanner_state_id == 0 &&
    ts_subtree_can_inline(padding, size)
  ) {
    uint64_t inline_flags =
      (extra ? SUBTREE_INLINE_EXTRA : 0ULL) |
      (has_external_tokens ? SUBTREE_INLINE_EXTERNAL : 0ULL);
    return ts_subtree_pack_inline(symbol, padding, size, metadata, inline_flags);
  }

  SubtreeHeapData *data = ts_subtree_pool_alloc_node(pool, 0);
  uint8_t flags =
    (metadata.visible ? SUBTREE_FLAG_VISIBLE : 0u) |
    (metadata.named ? SUBTREE_FLAG_NAMED : 0u) |
    (extra ? SUBTREE_FLAG_EXTRA : 0u) |
    (has_external_tokens ? SUBTREE_FLAG_HAS_EXTERNAL_TOKENS : 0u);
  *data = (SubtreeHeapData) {
    .padding = padding,
    .size = size,
    .error_cost = 0,
    .child_count = 0,
    .symbol = symbol,
    .production_id = 0,
    .repeat_depth = 0,
    .ref_count = 1,
    .flags = flags,
    .lookahead_char = 0,
    .scanner_state_id = scanner_state_id,
  };
  return ts_subtree_from_ptr(data);
}

void ts_subtree_set_symbol(
  SubtreePool *pool,
  MutableSubtree *self,
  TSSymbol symbol,
  const TSLanguage *language
) {
  TSSymbolMetadata metadata = ts_language_symbol_metadata(language, symbol);
  if (ts_mutable_subtree_is_inline(*self)) {
    Subtree current = ts_subtree_from_mut(*self);
    if (symbol <= TS_MAX_INLINE_TREE_SYMBOL) {
      uint64_t preserved = ts_subtree_inline_raw(current.bits) &
        (SUBTREE_INLINE_EXTRA | SUBTREE_INLINE_MISSING | SUBTREE_INLINE_EXTERNAL);
      *self = ts_subtree_to_mut_unsafe(ts_subtree_pack_inline(
        symbol, ts_subtree_padding(current), ts_subtree_size(current), metadata, preserved
      ));
      return;
    }
    Subtree spilled = ts_subtree_new_leaf(
      pool,
      symbol,
      ts_subtree_padding(current),
      ts_subtree_size(current),
      ts_subtree_has_external_tokens(current),
      0,
      language
    );
    MutableSubtree mut_spilled = ts_subtree_to_mut_unsafe(spilled);
    ts_subtree_heap_set_flag(mut_spilled.ptr, SUBTREE_FLAG_EXTRA, ts_subtree_extra(current));
    ts_subtree_heap_set_flag(mut_spilled.ptr, SUBTREE_FLAG_MISSING, ts_subtree_missing(current));
    *self = mut_spilled;
  } else {
    self->ptr->symbol = symbol;
    ts_subtree_heap_set_flag(self->ptr, SUBTREE_FLAG_NAMED, metadata.named);
    ts_subtree_heap_set_flag(self->ptr, SUBTREE_FLAG_VISIBLE, metadata.visible);
  }
}

Subtree ts_subtree_new_error(
  SubtreePool *pool, int32_t lookahead_char, Length padding, Length size,
  const TSLanguage *language
) {
  Subtree result = ts_subtree_new_leaf(
    pool, ts_builtin_sym_error, padding, size, false, 0, language
  );
  SubtreeHeapData *data = (SubtreeHeapData *)result.ptr;
  data->lookahead_char = lookahead_char;
  return result;
}

static MutableSubtree ts_subtree_clone(SubtreePool *pool, Subtree self) {
  uint32_t child_count = self.ptr->child_count;
  SubtreeHeapData *result = ts_subtree_pool_alloc_node(pool, child_count);
  Subtree *new_children = (Subtree *)result - child_count;
  const Subtree *old_children = ts_subtree_children(self);
  memcpy(new_children, old_children, ts_subtree_alloc_size(child_count));
  for (uint32_t i = 0; i < child_count; i++) {
    ts_subtree_retain(new_children[i]);
  }
  result->ref_count = 1;
  return ts_mutable_subtree_from_ptr(result);
}

MutableSubtree ts_subtree_make_mut(SubtreePool *pool, Subtree self) {
  if (ts_subtree_is_inline(self)) return ts_subtree_to_mut_unsafe(self);
  if (ts_subtree_heap_ref_count(self.ptr) == 1) return ts_subtree_to_mut_unsafe(self);
  MutableSubtree result = ts_subtree_clone(pool, self);
  ts_subtree_release(pool, self);
  return result;
}

void ts_subtree_compress(
  MutableSubtree self,
  unsigned count,
  const TSLanguage *language,
  MutableSubtreeArray *stack
) {
  unsigned initial_stack_size = stack->size;

  MutableSubtree tree = self;
  TSSymbol symbol = tree.ptr->symbol;
  for (unsigned i = 0; i < count; i++) {
    if (ts_subtree_heap_ref_count(tree.ptr) > 1 || tree.ptr->child_count < 2) break;

    MutableSubtree child = ts_subtree_to_mut_unsafe(ts_subtree_children(tree)[0]);
    if (
      ts_mutable_subtree_is_inline(child) ||
      child.ptr->child_count < 2 ||
      ts_subtree_heap_ref_count(child.ptr) > 1 ||
      child.ptr->symbol != symbol
    ) break;

    MutableSubtree grandchild = ts_subtree_to_mut_unsafe(ts_subtree_children(child)[0]);
    if (
      ts_mutable_subtree_is_inline(grandchild) ||
      grandchild.ptr->child_count < 2 ||
      ts_subtree_heap_ref_count(grandchild.ptr) > 1 ||
      grandchild.ptr->symbol != symbol
    ) break;

    ts_subtree_children(tree)[0] = ts_subtree_from_mut(grandchild);
    ts_subtree_children(child)[0] = ts_subtree_children(grandchild)[grandchild.ptr->child_count - 1];
    ts_subtree_children(grandchild)[grandchild.ptr->child_count - 1] = ts_subtree_from_mut(child);
    array_push(stack, tree);
    tree = grandchild;
  }

  while (stack->size > initial_stack_size) {
    tree = array_pop(stack);
    MutableSubtree child = ts_subtree_to_mut_unsafe(ts_subtree_children(tree)[0]);
    MutableSubtree grandchild = ts_subtree_to_mut_unsafe(
      ts_subtree_children(child)[child.ptr->child_count - 1]
    );
    ts_subtree_summarize_children(grandchild, language);
    ts_subtree_summarize_children(child, language);
    ts_subtree_summarize_children(tree, language);
  }
}

typedef struct {
  const TSLanguage *language;
  const TSSymbol *alias_sequence;
  uint32_t structural_index;
  bool charges_error_extent;
  bool is_extra_group;
} ChildSummary;

static inline void ts_subtree__summarize_extent(
  SubtreeHeapData *node,
  bool is_first,
  Length padding,
  Length size
) {
  if (is_first) {
    node->padding = padding;
    node->size = size;
  } else {
    node->size = length_add(node->size, length_add(padding, size));
  }
}

static inline bool ts_subtree__summarize_visibility(
  SubtreeHeapData *node,
  const ChildSummary *summary,
  TSSymbol symbol,
  bool extra,
  bool visible,
  bool named
) {
  TSSymbol alias = !extra && symbol != 0 && summary->alias_sequence
    ? summary->alias_sequence[summary->structural_index]
    : 0;
  if (alias) {
    named = ts_language_symbol_metadata(summary->language, alias).named;
  } else if (!visible) {
    return false;
  }
  node->visible_descendant_count++;
  node->visible_child_count++;
  if (named) node->named_child_count++;
  return true;
}

static inline void ts_subtree__summarize_inline_child(
  SubtreeHeapData *node,
  const ChildSummary *summary,
  Subtree child,
  bool is_first
) {
  Length padding = ts_subtree_padding(child);
  Length size = ts_subtree_size(child);
  bool extra = ts_subtree_extra(child);
  bool visible = ts_subtree_visible(child);
  bool named = ts_subtree_named(child);
  bool has_external = ts_subtree_has_external_tokens(child);
  ts_subtree__summarize_extent(node, is_first, padding, size);
  if (ts_subtree_missing(child)) node->error_cost += ERROR_COST_PER_MISSING_TREE + ERROR_COST_PER_RECOVERY;
  if (summary->charges_error_extent && !extra && visible) {
    node->error_cost += ERROR_COST_PER_SKIPPED_TREE;
  }
  ts_subtree__summarize_visibility(node, summary, ts_subtree_symbol(child), extra, visible, named);
  if (has_external) ts_subtree_heap_set_flag(node, SUBTREE_FLAG_HAS_EXTERNAL_TOKENS, true);
  if (padding.bytes || size.bytes || has_external || extra) {
    ts_subtree_heap_set_flag(node, SUBTREE_FLAG_TRAILING_EXTRA, extra);
  }
}

static inline void ts_subtree__summarize_error_cost(
  SubtreeHeapData *node,
  const ChildSummary *summary,
  Subtree child
) {
  const SubtreeHeapData *data = child.ptr;
  uint32_t grandchild_count = data->child_count;
  bool extra = ts_subtree_heap_flag(data, SUBTREE_FLAG_EXTRA);
  if (data->symbol == ts_builtin_sym_error_repeat && !extra) {
    uint32_t extent_cost = ts_subtree__error_extent_cost(data->size);
    ts_assert(ts_subtree_error_cost(child) >= extent_cost);
    node->error_cost += ts_subtree_error_cost(child) - extent_cost;
  } else {
    node->error_cost += ts_subtree_error_cost(child);
    if (
      summary->charges_error_extent && !extra &&
      !(data->symbol == ts_builtin_sym_error && grandchild_count == 0)
    ) {
      if (ts_subtree_heap_flag(data, SUBTREE_FLAG_VISIBLE)) {
        node->error_cost += ERROR_COST_PER_SKIPPED_TREE;
      } else if (grandchild_count > 0) {
        node->error_cost += ERROR_COST_PER_SKIPPED_TREE * data->visible_child_count;
      }
    }
  }
}

static void ts_subtree__summarize_heap_child(
  SubtreeHeapData *node,
  const ChildSummary *summary,
  Subtree child,
  bool is_first
) {
  const SubtreeHeapData *data = child.ptr;
  ts_subtree__summarize_extent(node, is_first, data->padding, data->size);
  ts_subtree__summarize_error_cost(node, summary, child);

  uint32_t grandchild_count = data->child_count;
  if (!summary->is_extra_group) {
    node->dynamic_precedence += ts_subtree_dynamic_precedence(child);
  }
  node->visible_descendant_count += ts_subtree_visible_descendant_count(child);
  bool extra = ts_subtree_heap_flag(data, SUBTREE_FLAG_EXTRA);
  bool visible = ts_subtree_heap_flag(data, SUBTREE_FLAG_VISIBLE);
  bool named = ts_subtree_heap_flag(data, SUBTREE_FLAG_NAMED);
  if (
    !ts_subtree__summarize_visibility(node, summary, data->symbol, extra, visible, named) &&
    grandchild_count > 0
  ) {
    node->visible_child_count += data->visible_child_count;
    node->named_child_count += data->named_child_count;
  }

  bool has_external = ts_subtree_heap_flag(data, SUBTREE_FLAG_HAS_EXTERNAL_TOKENS);
  if (has_external) ts_subtree_heap_set_flag(node, SUBTREE_FLAG_HAS_EXTERNAL_TOKENS, true);
  if (ts_subtree_heap_flag(data, SUBTREE_FLAG_UNBALANCED_REPEAT)) {
    ts_subtree_heap_set_flag(node, SUBTREE_FLAG_UNBALANCED_REPEAT, true);
  }
  if (ts_subtree_total_bytes(child) || has_external || extra) {
    ts_subtree_heap_set_flag(node, SUBTREE_FLAG_TRAILING_EXTRA, ts_subtree_trailing_extra(child));
  }
}

static inline void ts_subtree__summarize_ends(
  SubtreeHeapData *node,
  Subtree first_child,
  Subtree last_child
) {
  ts_subtree_heap_set_flag(
    node,
    SUBTREE_FLAG_COMPLETED_PREFIX,
    ts_subtree_has_completed_prefix(first_child)
  );

  if (
    node->child_count >= 2 &&
    !ts_subtree_heap_flag(node, SUBTREE_FLAG_EXTRA_GROUP) &&
    !ts_subtree_heap_flag(node, SUBTREE_FLAG_VISIBLE) &&
    !ts_subtree_heap_flag(node, SUBTREE_FLAG_NAMED) &&
    ts_subtree_symbol(first_child) == node->symbol
  ) {
    uint16_t first_depth = (uint16_t)ts_subtree_repeat_depth(first_child);
    uint16_t last_depth = (uint16_t)ts_subtree_repeat_depth(last_child);
    node->repeat_depth = (uint16_t)((first_depth > last_depth ? first_depth : last_depth) + 1);
    if ((uint32_t)first_depth > (uint32_t)last_depth + 1u) {
      ts_subtree_heap_set_flag(node, SUBTREE_FLAG_UNBALANCED_REPEAT, true);
    }
  }
}

static void ts_subtree__reset_child_summary_fields(SubtreeHeapData *node) {
  node->named_child_count = 0;
  node->visible_child_count = 0;
  node->error_cost = 0;
  node->repeat_depth = 0;
  node->visible_descendant_count = 0;
  if (!ts_subtree_heap_flag(node, SUBTREE_FLAG_EXTRA_GROUP)) {
    node->dynamic_precedence = 0;
  }
  node->flags &= (uint8_t)~(
    SUBTREE_FLAG_HAS_EXTERNAL_TOKENS |
    SUBTREE_FLAG_TRAILING_EXTRA |
    SUBTREE_FLAG_COMPLETED_PREFIX |
    SUBTREE_FLAG_UNBALANCED_REPEAT
  );
}

void ts_subtree_summarize_children(
  MutableSubtree self,
  const TSLanguage *language
) {
  ts_assert(!ts_mutable_subtree_is_inline(self));
  ts_subtree__reset_child_summary_fields(self.ptr);

  bool is_extra_group = ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_EXTRA_GROUP);
  bool charges_error_extent = self.ptr->symbol == ts_builtin_sym_error ||
    (self.ptr->symbol == ts_builtin_sym_error_repeat && !ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_EXTRA));
  ChildSummary summary = {
    .language = language,
    .alias_sequence = ts_language_alias_sequence(language, ts_subtree_production_id(ts_subtree_from_mut(self))),
    .structural_index = 0,
    .charges_error_extent = charges_error_extent,
    .is_extra_group = is_extra_group,
  };

  const Subtree *children = ts_subtree_children(self);
  for (uint32_t i = 0; i < self.ptr->child_count; i++) {
    Subtree child = children[i];
    if (ts_subtree_is_inline(child)) {
      ts_subtree__summarize_inline_child(self.ptr, &summary, child, i == 0);
    } else {
      ts_subtree__summarize_heap_child(self.ptr, &summary, child, i == 0);
    }
    if (!ts_subtree_extra(child)) summary.structural_index++;
  }

  if (charges_error_extent) {
    self.ptr->error_cost += ts_subtree__error_extent_cost(self.ptr->size);
  }

  if (self.ptr->child_count > 0) {
    ts_subtree__summarize_ends(self.ptr, children[0], children[self.ptr->child_count - 1]);
  }
}

Subtree ts_subtree_new_extra_group(
  SubtreePool *pool,
  Subtree left,
  Subtree right,
  const TSLanguage *language
) {
  ts_assert(ts_subtree_extra(left) && ts_subtree_extra(right));
  SubtreeHeapData *data = ts_subtree_pool_alloc_node(pool, 2);
  Subtree *children = (Subtree *)data - 2;
  children[0] = left;
  children[1] = right;
  *data = (SubtreeHeapData) {
    .symbol = ts_builtin_sym_error_repeat,
    .child_count = 2,
    .production_id = 0,
    .ref_count = 1,
    .flags = SUBTREE_FLAG_EXTRA | SUBTREE_FLAG_EXTRA_GROUP,
    .stack_link_count = ts_subtree_stack_link_count(left) + ts_subtree_stack_link_count(right),
  };
  MutableSubtree result = ts_mutable_subtree_from_ptr(data);
  ts_subtree_summarize_children(result, language);
  return ts_subtree_from_mut(result);
}

MutableSubtree ts_subtree_finish_node(
  SubtreeHeapData *data,
  TSSymbol symbol,
  uint32_t child_count,
  unsigned production_id,
  const TSLanguage *language
) {
  TSSymbolMetadata metadata = ts_language_symbol_metadata(language, symbol);
  uint8_t flags =
    (metadata.visible ? SUBTREE_FLAG_VISIBLE : 0u) |
    (metadata.named ? SUBTREE_FLAG_NAMED : 0u);
  *data = (SubtreeHeapData) {
    .symbol = symbol,
    .child_count = child_count,
    .production_id = (uint16_t)production_id,
    .ref_count = 1,
    .flags = flags,
  };
  MutableSubtree result = ts_mutable_subtree_from_ptr(data);
  ts_subtree_summarize_children(result, language);
  return result;
}

MutableSubtree ts_subtree_new_node(
  SubtreePool *pool,
  TSSymbol symbol,
  SubtreeArray *children,
  unsigned production_id,
  const TSLanguage *language
) {
  uint32_t child_count = children->size;
  SubtreeHeapData *data = ts_subtree_pool_alloc_node(pool, child_count);
  if (child_count > 0) {
    Subtree *dest = (Subtree *)data - child_count;
    memcpy(dest, children->contents, (size_t)child_count * sizeof(Subtree));
  }
  array_delete(children);
  return ts_subtree_finish_node(data, symbol, child_count, production_id, language);
}

MutableSubtree ts_subtree_init_scratch_node(
  TSSymbol symbol,
  SubtreeArray *scratch_trees,
  unsigned production_id,
  const TSLanguage *language
) {
  size_t new_byte_size = ts_subtree_alloc_size(scratch_trees->size);
  if (scratch_trees->capacity * sizeof(Subtree) < new_byte_size) {
    scratch_trees->contents = ts_realloc(scratch_trees->contents, new_byte_size);
    scratch_trees->capacity = (uint32_t)(new_byte_size / sizeof(Subtree));
  }
  SubtreeHeapData *data = (SubtreeHeapData *)&scratch_trees->contents[scratch_trees->size];
  return ts_subtree_finish_node(data, symbol, scratch_trees->size, production_id, language);
}

Subtree ts_subtree_new_error_node(
  SubtreePool *pool,
  SubtreeArray *children,
  bool extra,
  const TSLanguage *language
) {
  MutableSubtree result = ts_subtree_new_node(
    pool, ts_builtin_sym_error, children, 0, language
  );
  ts_subtree_heap_set_flag(result.ptr, SUBTREE_FLAG_EXTRA, extra);
  return ts_subtree_from_mut(result);
}

Subtree ts_subtree_new_missing_leaf(
  SubtreePool *pool,
  TSSymbol symbol,
  Length padding,
  const TSLanguage *language
) {
  Subtree result = ts_subtree_new_leaf(
    pool, symbol, padding, length_zero(), false, 0, language
  );
  if (ts_subtree_is_inline(result)) {
    uint64_t raw = ts_subtree_inline_raw(result.bits) | SUBTREE_INLINE_MISSING;
    result.bits = ts_subtree_inline_raw(raw);
  } else {
    ts_subtree_heap_set_flag((SubtreeHeapData *)result.ptr, SUBTREE_FLAG_MISSING, true);
  }
  return result;
}

void ts_subtree_retain(Subtree self) {
  if (ts_subtree_is_inline(self)) return;
  SubtreeHeapData *data = (SubtreeHeapData *)self.ptr;
  ts_assert(data->ref_count > 0);
  if (data->ref_count < SUBTREE_REF_MAX) {
    data->ref_count++;
  }
}

static inline bool ts_subtree_dec_is_last(SubtreeHeapData *data) {
  ts_assert(data->ref_count > 0);
  if (data->ref_count == SUBTREE_REF_MAX) return false;
  return --data->ref_count == 0;
}

void ts_subtree_release(SubtreePool *pool, Subtree self) {
  if (ts_subtree_is_inline(self)) return;
  if (!ts_subtree_dec_is_last((SubtreeHeapData *)self.ptr)) return;

  array_clear(&pool->tree_stack);
  array_push(&pool->tree_stack, ts_subtree_to_mut_unsafe(self));
  while (pool->tree_stack.size > 0) {
    MutableSubtree tree = array_pop(&pool->tree_stack);
    uint32_t child_count = tree.ptr->child_count;
    if (child_count > 0) {
      Subtree *children = ts_subtree_children(tree);
      for (uint32_t i = 0; i < child_count; i++) {
        Subtree child = children[i];
        if (!ts_subtree_is_inline(child) && ts_subtree_dec_is_last((SubtreeHeapData *)child.ptr)) {
          array_push(&pool->tree_stack, ts_subtree_to_mut_unsafe(child));
        }
      }
    }
    ts_subtree_pool_free_node(pool, tree.ptr);
  }
}

int ts_subtree_compare(Subtree left, Subtree right, SubtreePool *pool) {
  array_push(&pool->tree_stack, ts_subtree_to_mut_unsafe(left));
  array_push(&pool->tree_stack, ts_subtree_to_mut_unsafe(right));

  while (pool->tree_stack.size > 0) {
    right = ts_subtree_from_mut(array_pop(&pool->tree_stack));
    left = ts_subtree_from_mut(array_pop(&pool->tree_stack));

    int result = 0;
    if (ts_subtree_symbol(left) < ts_subtree_symbol(right)) result = -1;
    else if (ts_subtree_symbol(right) < ts_subtree_symbol(left)) result = 1;
    else if (ts_subtree_child_count(left) < ts_subtree_child_count(right)) result = -1;
    else if (ts_subtree_child_count(right) < ts_subtree_child_count(left)) result = 1;
    if (result != 0) {
      array_clear(&pool->tree_stack);
      return result;
    }

    for (uint32_t i = ts_subtree_child_count(left); i > 0; i--) {
      Subtree left_child = ts_subtree_children(left)[i - 1];
      Subtree right_child = ts_subtree_children(right)[i - 1];
      array_push(&pool->tree_stack, ts_subtree_to_mut_unsafe(left_child));
      array_push(&pool->tree_stack, ts_subtree_to_mut_unsafe(right_child));
    }
  }

  return 0;
}

static size_t ts_subtree__write_char_to_string(char *str, size_t n, int32_t chr) {
  if (chr == -1)
    return snprintf(str, n, "INVALID");
  else if (chr == '\0')
    return snprintf(str, n, "'\\0'");
  else if (chr == '\n')
    return snprintf(str, n, "'\\n'");
  else if (chr == '\t')
    return snprintf(str, n, "'\\t'");
  else if (chr == '\r')
    return snprintf(str, n, "'\\r'");
  else if (0 < chr && chr < 128 && isprint(chr))
    return snprintf(str, n, "'%c'", chr);
  else
    return snprintf(str, n, "%d", chr);
}

static const char *const ROOT_FIELD = "__ROOT__";

typedef struct {
  Subtree subtree;
  TSSymbol alias_symbol;
  bool alias_is_named;
  const char *field_name;
  bool is_root;

  bool pre_written;
  bool is_visible;
  uint32_t child_index;
  uint32_t structural_child_index;
  const TSSymbol *alias_sequence;
  const TSFieldMapEntry *field_map;
  const TSFieldMapEntry *field_map_end;
} WriteToStringFrame;

static size_t ts_subtree__write_indent(char *string, size_t limit, size_t indent_level);

// code-gauge-allow functionParameterCount -- iterative S-expression formatter threads output buffer and root metadata
static size_t ts_subtree__write_to_string(
  Subtree self, char *string, size_t limit,
  const TSLanguage *language, SubtreeStringOptions options,
  TSSymbol root_alias_symbol, bool root_alias_is_named, const char *root_field_name
) {
  char *cursor = string;
  char **writer = (limit > 1) ? &cursor : &string;
  size_t indent_level = options.initial_indent_level;
  if (options.pretty && indent_level > 0) {
    cursor += ts_subtree__write_indent(*writer, limit, indent_level);
  }

  Array(WriteToStringFrame) stack = array_new();
  array_push(&stack, ((WriteToStringFrame) {
    .subtree = self,
    .alias_symbol = root_alias_symbol,
    .alias_is_named = root_alias_is_named,
    .field_name = root_field_name,
    .is_root = root_field_name == ROOT_FIELD,
  }));

  while (stack.size) {
    WriteToStringFrame *frame = array_back(&stack);
    Subtree node = frame->subtree;

    if (!node.ptr) {
      if (!frame->is_root) {
        cursor += options.pretty
          ? ts_subtree__write_indent(*writer, limit, indent_level)
          : snprintf(*writer, limit, " ");
        if (frame->field_name) {
          cursor += snprintf(*writer, limit, "%s: ", frame->field_name);
        }
      }
      cursor += snprintf(*writer, limit, "(NULL)");
      (void)array_pop(&stack);
      continue;
    }

    if (!frame->pre_written) {
      bool is_visible =
        options.include_all ||
        ts_subtree_missing(node) ||
        (
          frame->alias_symbol
            ? frame->alias_is_named
            : ts_subtree_visible(node) && ts_subtree_named(node)
        );

      if (is_visible) {
        if (!frame->is_root) {
          cursor += options.pretty
            ? ts_subtree__write_indent(*writer, limit, indent_level)
            : snprintf(*writer, limit, " ");
          if (frame->field_name) {
            cursor += snprintf(*writer, limit, "%s: ", frame->field_name);
          }
        }

        if (ts_subtree_is_error(node) && ts_subtree_child_count(node) == 0 && node.ptr->size.bytes > 0) {
          cursor += snprintf(*writer, limit, "(UNEXPECTED ");
          cursor += ts_subtree__write_char_to_string(*writer, limit, node.ptr->lookahead_char);
        } else {
          TSSymbol symbol = frame->alias_symbol ? frame->alias_symbol : ts_subtree_symbol(node);
          const char *symbol_name = ts_language_symbol_name(language, symbol);
          if (ts_subtree_missing(node)) {
            cursor += snprintf(*writer, limit, "(MISSING ");
            if (frame->alias_is_named || ts_subtree_named(node)) {
              cursor += snprintf(*writer, limit, "%s", symbol_name);
            } else {
              cursor += snprintf(*writer, limit, "\"%s\"", symbol_name);
            }
          } else {
            cursor += snprintf(*writer, limit, "(%s", symbol_name);
          }
        }
      } else if (frame->is_root) {
        TSSymbol symbol = frame->alias_symbol ? frame->alias_symbol : ts_subtree_symbol(node);
        const char *symbol_name = ts_language_symbol_name(language, symbol);
        if (ts_subtree_child_count(node) > 0) {
          cursor += snprintf(*writer, limit, "(%s", symbol_name);
        } else if (ts_subtree_named(node)) {
          cursor += snprintf(*writer, limit, "(%s)", symbol_name);
        } else {
          cursor += snprintf(*writer, limit, "(\"%s\")", symbol_name);
        }
      }

      if (ts_subtree_child_count(node)) {
        uint16_t production_id = ts_subtree_production_id(node);
        frame->alias_sequence = ts_language_alias_sequence(language, production_id);
        ts_language_field_map(
          language,
          production_id,
          &frame->field_map,
          &frame->field_map_end
        );
      }

      frame->is_visible = is_visible;
      if (options.pretty && is_visible) indent_level++;
      frame->pre_written = true;
    }

    if (frame->child_index < ts_subtree_child_count(node)) {
      Subtree child = ts_subtree_children(node)[frame->child_index];
      WriteToStringFrame child_frame = {
        .subtree = child,
        .is_root = false,
      };

      if (!ts_subtree_extra(child)) {
        TSSymbol subtree_alias_symbol = frame->alias_sequence
          ? frame->alias_sequence[frame->structural_child_index]
          : 0;
        bool subtree_alias_is_named = subtree_alias_symbol
          ? ts_language_symbol_metadata(language, subtree_alias_symbol).named
          : false;

        const char *child_field_name = frame->is_visible ? NULL : frame->field_name;
        for (const TSFieldMapEntry *map = frame->field_map; map < frame->field_map_end; map++) {
          if (!map->inherited && map->child_index == frame->structural_child_index) {
            child_field_name = ts_language_field_name_for_id(language, map->field_id);
            break;
          }
        }

        child_frame.alias_symbol = subtree_alias_symbol;
        child_frame.alias_is_named = subtree_alias_is_named;
        child_frame.field_name = child_field_name;
        frame->structural_child_index++;
      }

      frame->child_index++;
      array_push(&stack, child_frame);
      continue;
    }

    if (frame->is_visible) {
      cursor += snprintf(*writer, limit, ")");
      if (options.pretty) indent_level--;
    }
    (void)array_pop(&stack);
  }

  array_delete(&stack);
  return cursor - string;
}

static size_t ts_subtree__write_indent(char *string, size_t limit, size_t indent_level) {
  size_t size = snprintf(string, limit, "\n");
  for (size_t i = 0; i < indent_level; i++) {
    char *position = limit > 1 ? string + size : string;
    size += snprintf(position, limit, "  ");
  }
  return size;
}

char *ts_subtree_string(
  Subtree self,
  TSSymbol alias_symbol,
  bool alias_is_named,
  const TSLanguage *language,
  SubtreeStringOptions options
) {
  char scratch_string[1];
  size_t size = ts_subtree__write_to_string(
    self, scratch_string, 1,
    language, options,
    alias_symbol, alias_is_named, ROOT_FIELD
  ) + 1;
  char *result = ts_malloc(size * sizeof(char));
  ts_subtree__write_to_string(
    self, result, size,
    language, options,
    alias_symbol, alias_is_named, ROOT_FIELD
  );
  return result;
}

typedef struct {
  const Subtree *subtree;
  uint32_t start_offset;
  uint32_t child_start_offset;
  uint32_t child_info_offset;
  uint32_t child_index;
  TSSymbol alias_symbol;
} PrintDotGraphFrame;

static void ts_subtree__write_dot_node(
  const Subtree *self,
  uint32_t start_offset,
  const TSLanguage *language,
  TSSymbol alias_symbol,
  FILE *f
);

static void ts_subtree__print_dot_graph(
  const Subtree *self,
  uint32_t start_offset,
  const TSLanguage *language,
  TSSymbol alias_symbol,
  FILE *f
) {
  Array(PrintDotGraphFrame) stack = array_new();
  array_push(&stack, ((PrintDotGraphFrame) {
    .subtree = self,
    .start_offset = start_offset,
    .child_start_offset = start_offset,
    .child_info_offset = ts_language_alias_sequence_offset(language, ts_subtree_production_id(*self)),
    .alias_symbol = alias_symbol,
  }));

  while (stack.size) {
    PrintDotGraphFrame *frame = array_back(&stack);
    self = frame->subtree;
    if (frame->child_index == 0) {
      ts_subtree__write_dot_node(self, frame->start_offset, language, frame->alias_symbol, f);
    }

    if (frame->child_index < ts_subtree_child_count(*self)) {
      const Subtree *child = &ts_subtree_children(*self)[frame->child_index];
      TSSymbol child_alias_symbol = 0;
      if (!ts_subtree_extra(*child) && frame->child_info_offset) {
        child_alias_symbol = language->alias_sequences[frame->child_info_offset++];
      }
      uint32_t child_start_offset = frame->child_start_offset;
      frame->child_start_offset += ts_subtree_total_bytes(*child);
      frame->child_index++;
      array_push(&stack, ((PrintDotGraphFrame) {
        .subtree = child,
        .start_offset = child_start_offset,
        .child_start_offset = child_start_offset,
        .child_info_offset = ts_language_alias_sequence_offset(language, ts_subtree_production_id(*child)),
        .alias_symbol = child_alias_symbol,
      }));
    } else {
      (void)array_pop(&stack);
      if (stack.size) {
        frame = array_back(&stack);
        fprintf(f, "tree_%p -> tree_%p [tooltip=%u]\n",
          (const void *)frame->subtree, (const void *)self, frame->child_index - 1);
      }
    }
  }

  array_delete(&stack);
}

static void ts_subtree__write_dot_node(
  const Subtree *self,
  uint32_t start_offset,
  const TSLanguage *language,
  TSSymbol alias_symbol,
  FILE *f
) {
  TSSymbol subtree_symbol = ts_subtree_symbol(*self);
  TSSymbol symbol = alias_symbol ? alias_symbol : subtree_symbol;
  uint32_t end_offset = start_offset + ts_subtree_total_bytes(*self);
  fprintf(f, "tree_%p [label=\"", (const void *)self);
  ts_language_write_symbol_as_dot_string(language, f, symbol);
  fprintf(f, "\"");

  if (ts_subtree_child_count(*self) == 0) fprintf(f, ", shape=plaintext");
  if (ts_subtree_extra(*self)) fprintf(f, ", fontcolor=gray");

  fprintf(f, ", tooltip=\""
    "range: %u - %u\n"
    "error-cost: %u\n"
    "descendant-count: %u\n"
    "repeat-depth: %u",
    start_offset, end_offset,
    ts_subtree_error_cost(*self),
    ts_subtree_visible_descendant_count(*self),
    ts_subtree_repeat_depth(*self)
  );

  if (ts_subtree_is_error(*self) && ts_subtree_child_count(*self) == 0 && self->ptr->lookahead_char != 0) {
    fprintf(f, "\ncharacter: '%c'", self->ptr->lookahead_char);
  }

  fprintf(f, "\"]\n");
}

void ts_subtree_print_dot_graph(Subtree self, const TSLanguage *language, FILE *f) {
  fprintf(f, "digraph tree {\n");
  fprintf(f, "edge [arrowhead=none]\n");
  ts_subtree__print_dot_graph(&self, 0, language, 0, f);
  fprintf(f, "}\n");
}
