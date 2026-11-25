#ifndef TREE_SITTER_SUBTREE_H_
#define TREE_SITTER_SUBTREE_H_

#ifdef __cplusplus
extern "C" {
#endif

#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include "./length.h"
#include "./array.h"
#include "./error_costs.h"
#include "./host.h"
#include "tree_sitter/api.h"
#include "./parser.h"

#define TS_TREE_STATE_NONE USHRT_MAX
#define NULL_SUBTREE ((Subtree) {.bits = 0})

#define SUBTREE_INLINE_BIT 1ULL
#define SUBTREE_INLINE_VISIBLE (1ULL << 1)
#define SUBTREE_INLINE_NAMED (1ULL << 2)
#define SUBTREE_INLINE_EXTRA (1ULL << 3)
#define SUBTREE_INLINE_MISSING (1ULL << 4)
#define SUBTREE_INLINE_EXTERNAL (1ULL << 5)

#define SUBTREE_INLINE_SYMBOL_SHIFT 6
#define SUBTREE_INLINE_SYMBOL_MASK 0x1FFFULL
#define SUBTREE_INLINE_SIZE_SHIFT 19
#define SUBTREE_INLINE_SIZE_MASK 0xFFFULL
#define SUBTREE_INLINE_PAD_BYTES_SHIFT 31
#define SUBTREE_INLINE_PAD_BYTES_MASK 0xFFFULL
#define SUBTREE_INLINE_PAD_ROWS_SHIFT 43
#define SUBTREE_INLINE_PAD_ROWS_MASK 0x1FFULL
#define SUBTREE_INLINE_PAD_COLS_SHIFT 52
#define SUBTREE_INLINE_PAD_COLS_MASK 0xFFFULL

#define TS_MAX_INLINE_TREE_SYMBOL 0x1FFFu
#define TS_MAX_INLINE_TREE_LENGTH 0x1000u
#define TS_MAX_INLINE_TREE_ROWS 0x200u

#define SUBTREE_REF_MAX UINT8_MAX
#define SUBTREE_FLAG_VISIBLE (1u << 0)
#define SUBTREE_FLAG_NAMED (1u << 1)
#define SUBTREE_FLAG_EXTRA (1u << 2)
#define SUBTREE_FLAG_HAS_EXTERNAL_TOKENS (1u << 3)
// Missing nodes are always leaves (`child_count == 0`); completed-prefix is internal-only (`child_count > 0`).
#define SUBTREE_FLAG_MISSING (1u << 4)
#define SUBTREE_FLAG_COMPLETED_PREFIX (1u << 4)
#define SUBTREE_FLAG_TRAILING_EXTRA (1u << 5)
#define SUBTREE_FLAG_EXTRA_GROUP (1u << 6)
#define SUBTREE_FLAG_UNBALANCED_REPEAT (1u << 7)

typedef struct {
  Length padding;
  Length size;
  uint32_t error_cost;
  uint32_t child_count;
  TSSymbol symbol;
  uint16_t production_id;
  uint16_t repeat_depth;
  uint8_t ref_count;
  uint8_t flags;

  union {
    struct {
      uint32_t visible_child_count;
      uint32_t named_child_count;
      uint32_t visible_descendant_count;
      union {
        int32_t dynamic_precedence;
        uint32_t stack_link_count;
      };
    };
    struct {
      int32_t lookahead_char;
      uint32_t scanner_state_id;
    };
  };
} SubtreeHeapData;

typedef union {
  uint64_t bits;
  const SubtreeHeapData *ptr;
} Subtree;

typedef union {
  uint64_t bits;
  SubtreeHeapData *ptr;
} MutableSubtree;

typedef struct {
  Subtree subtree;
  TSStateId parse_state;
  bool is_keyword;
  bool is_keyword_exact;
  bool has_external_scanner_state_change;
} LookaheadToken;

#define NULL_LOOKAHEAD ((LookaheadToken) {.subtree = {.bits = 0}})

typedef Array(Subtree) SubtreeArray;
typedef Array(MutableSubtree) MutableSubtreeArray;

typedef struct TreeArenaLargeBlock {
  struct TreeArenaLargeBlock *prev;
  struct TreeArenaLargeBlock *next;
  uint64_t data[];
} TreeArenaLargeBlock;

typedef struct TreeArenaSlab {
  struct TreeArenaSlab *next;
  TreeArenaLargeBlock *large_blocks;
  volatile uint32_t ref_count;
  uint32_t used;
  uint64_t data[];
} TreeArenaSlab;

#define TREE_ARENA_BIN_COUNT 9

typedef struct {
  TreeArenaSlab *head;
  void *free_bins[TREE_ARENA_BIN_COUNT];
} TreeArena;

typedef struct {
  TreeArena arena;
  MutableSubtreeArray tree_stack;
} SubtreePool;

void ts_subtree_array_copy(SubtreeArray self, SubtreeArray *dest);
void ts_subtree_array_clear(SubtreePool *pool, SubtreeArray *self);
void ts_subtree_array_delete(SubtreePool *pool, SubtreeArray *self);
void ts_subtree_array_remove_trailing_extras(SubtreeArray *self, SubtreeArray *destination);
void ts_subtree_array_reverse(SubtreeArray *self);

SubtreePool ts_subtree_pool_new(uint32_t capacity);
void ts_subtree_pool_clear(SubtreePool *self);
void ts_subtree_pool_delete(SubtreePool *self);
TreeArenaSlab *ts_subtree_pool_take_arena(SubtreePool *self);
void ts_tree_arena_slab_free_all(TreeArenaSlab *head);
SubtreeHeapData *ts_subtree_pool_alloc_node(SubtreePool *self, uint32_t child_count);

Subtree ts_subtree_new_leaf(
  SubtreePool *pool, TSSymbol symbol, Length padding, Length size,
  bool has_external_tokens, uint32_t scanner_state_id, const TSLanguage *language
);
Subtree ts_subtree_new_error(
  SubtreePool *pool, int32_t lookahead_char, Length padding, Length size,
  const TSLanguage *language
);
Subtree ts_subtree_new_extra_group(
  SubtreePool *pool, Subtree left, Subtree right, const TSLanguage *language
);
MutableSubtree ts_subtree_finish_node(
  SubtreeHeapData *data,
  TSSymbol symbol,
  uint32_t child_count,
  unsigned production_id,
  const TSLanguage *language
);
MutableSubtree ts_subtree_new_node(
  SubtreePool *pool,
  TSSymbol symbol,
  SubtreeArray *children,
  unsigned production_id,
  const TSLanguage *language
);
MutableSubtree ts_subtree_init_scratch_node(
  TSSymbol symbol,
  SubtreeArray *scratch_trees,
  unsigned production_id,
  const TSLanguage *language
);
Subtree ts_subtree_new_error_node(
  SubtreePool *pool,
  SubtreeArray *children,
  bool extra,
  const TSLanguage *language
);
Subtree ts_subtree_new_missing_leaf(
  SubtreePool *pool,
  TSSymbol symbol,
  Length padding,
  const TSLanguage *language
);
MutableSubtree ts_subtree_make_mut(SubtreePool *pool, Subtree self);
void ts_subtree_retain(Subtree self);
void ts_subtree_release(SubtreePool *pool, Subtree self);
int ts_subtree_compare(Subtree left, Subtree right, SubtreePool *pool);
void ts_subtree_set_symbol(
  SubtreePool *pool, MutableSubtree *self, TSSymbol symbol, const TSLanguage *language
);
void ts_subtree_compress(
  MutableSubtree self, unsigned count, const TSLanguage *language, MutableSubtreeArray *stack
);
void ts_subtree_summarize_children(MutableSubtree self, const TSLanguage *language);

typedef struct {
  bool include_all;
  bool pretty;
  size_t initial_indent_level;
} SubtreeStringOptions;

char *ts_subtree_string(
  Subtree self, TSSymbol alias_symbol, bool alias_is_named,
  const TSLanguage *language, SubtreeStringOptions options
);
void ts_subtree_print_dot_graph(Subtree self, const TSLanguage *language, FILE *f);

static inline uint64_t ts_subtree_inline_raw(uint64_t bits) {
#if TS_BIG_ENDIAN && TS_PTR_SIZE == 32
  return (bits >> 32) | (bits << 32);
#else
  return bits;
#endif
}

static inline bool ts_subtree_is_inline(Subtree self) {
  return ((uintptr_t)self.ptr & 1u) != 0;
}

static inline bool ts_mutable_subtree_is_inline(MutableSubtree self) {
  return ((uintptr_t)self.ptr & 1u) != 0;
}

static inline Subtree ts_subtree_from_ptr(const SubtreeHeapData *ptr) {
  Subtree result = {.bits = 0};
  result.ptr = ptr;
  return result;
}

static inline MutableSubtree ts_mutable_subtree_from_ptr(SubtreeHeapData *ptr) {
  MutableSubtree result = {.bits = 0};
  result.ptr = ptr;
  return result;
}

static inline bool ts_subtree_heap_flag(const SubtreeHeapData *data, uint8_t flag) {
  return (data->flags & flag) != 0;
}

static inline void ts_subtree_heap_set_flag(SubtreeHeapData *data, uint8_t flag, bool value) {
  if (value) {
    data->flags |= flag;
  } else {
    data->flags &= (uint8_t)~flag;
  }
}

static inline uint8_t ts_subtree_heap_ref_count(const SubtreeHeapData *data) {
  return data->ref_count;
}

static inline TSSymbol ts_subtree_symbol(Subtree self) {
  return ts_subtree_is_inline(self)
    ? (TSSymbol)((ts_subtree_inline_raw(self.bits) >> SUBTREE_INLINE_SYMBOL_SHIFT) & SUBTREE_INLINE_SYMBOL_MASK)
    : self.ptr->symbol;
}

static inline bool ts_subtree_visible(Subtree self) {
  return ts_subtree_is_inline(self)
    ? (ts_subtree_inline_raw(self.bits) & SUBTREE_INLINE_VISIBLE) != 0
    : ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_VISIBLE);
}

static inline bool ts_subtree_named(Subtree self) {
  return ts_subtree_is_inline(self)
    ? (ts_subtree_inline_raw(self.bits) & SUBTREE_INLINE_NAMED) != 0
    : ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_NAMED);
}

static inline bool ts_subtree_extra(Subtree self) {
  return ts_subtree_is_inline(self)
    ? (ts_subtree_inline_raw(self.bits) & SUBTREE_INLINE_EXTRA) != 0
    : ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_EXTRA);
}

static inline bool ts_subtree_missing(Subtree self) {
  return ts_subtree_is_inline(self)
    ? (ts_subtree_inline_raw(self.bits) & SUBTREE_INLINE_MISSING) != 0
    : self.ptr->child_count == 0 && ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_MISSING);
}

static inline bool ts_subtree_has_external_tokens(Subtree self) {
  return ts_subtree_is_inline(self)
    ? (ts_subtree_inline_raw(self.bits) & SUBTREE_INLINE_EXTERNAL) != 0
    : ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_HAS_EXTERNAL_TOKENS);
}

static inline uint32_t ts_subtree_scanner_state_id(Subtree self) {
  if (
    self.ptr &&
    !ts_subtree_is_inline(self) &&
    self.ptr->child_count == 0 &&
    ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_HAS_EXTERNAL_TOKENS)
  ) {
    return self.ptr->scanner_state_id;
  }
  return 0;
}

static inline size_t ts_subtree_alloc_size(uint32_t child_count) {
  return child_count * sizeof(Subtree) + sizeof(SubtreeHeapData);
}

static inline bool ts_subtree_trailing_extra(Subtree self) {
  return ts_subtree_extra(self) || (
    !ts_subtree_is_inline(self) &&
    self.ptr->child_count > 0 &&
    ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_TRAILING_EXTRA)
  );
}

#define ts_subtree_children(self) \
  (ts_subtree_is_inline((Subtree) {.bits = (self).bits}) ? (Subtree *)NULL : (Subtree *)((self).ptr) - (self).ptr->child_count)

static inline void ts_subtree_set_extra(MutableSubtree *self, bool is_extra) {
  if (ts_mutable_subtree_is_inline(*self)) {
    uint64_t raw = ts_subtree_inline_raw(self->bits);
    raw = is_extra ? (raw | SUBTREE_INLINE_EXTRA) : (raw & ~SUBTREE_INLINE_EXTRA);
    self->bits = ts_subtree_inline_raw(raw);
  } else {
    ts_subtree_heap_set_flag(self->ptr, SUBTREE_FLAG_EXTRA, is_extra);
  }
}

static inline Length ts_subtree_padding(Subtree self) {
  if (ts_subtree_is_inline(self)) {
    uint64_t raw = ts_subtree_inline_raw(self.bits);
    Length result = {
      (uint32_t)((raw >> SUBTREE_INLINE_PAD_BYTES_SHIFT) & SUBTREE_INLINE_PAD_BYTES_MASK),
      {
        (uint32_t)((raw >> SUBTREE_INLINE_PAD_ROWS_SHIFT) & SUBTREE_INLINE_PAD_ROWS_MASK),
        (uint32_t)((raw >> SUBTREE_INLINE_PAD_COLS_SHIFT) & SUBTREE_INLINE_PAD_COLS_MASK),
      },
    };
    return result;
  }
  return self.ptr->padding;
}

static inline Length ts_subtree_size(Subtree self) {
  if (ts_subtree_is_inline(self)) {
    uint32_t bytes = (uint32_t)((ts_subtree_inline_raw(self.bits) >> SUBTREE_INLINE_SIZE_SHIFT) & SUBTREE_INLINE_SIZE_MASK);
    Length result = {bytes, {0, bytes}};
    return result;
  }
  return self.ptr->size;
}

static inline Length ts_subtree_total_size(Subtree self) {
  return length_add(ts_subtree_padding(self), ts_subtree_size(self));
}

static inline uint32_t ts_subtree_total_bytes(Subtree self) {
  return ts_subtree_total_size(self).bytes;
}

static inline uint32_t ts_subtree_child_count(Subtree self) {
  return ts_subtree_is_inline(self) ? 0 : self.ptr->child_count;
}

static inline uint32_t ts_subtree_stack_link_count(Subtree self) {
  return self.ptr && !ts_subtree_is_inline(self) && ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_EXTRA_GROUP)
    ? self.ptr->stack_link_count
    : 1;
}

static inline uint32_t ts_subtree_repeat_depth(Subtree self) {
  return ts_subtree_is_inline(self) || ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_EXTRA_GROUP)
    ? 0
    : self.ptr->repeat_depth;
}

static inline uint32_t ts_subtree_is_repetition(Subtree self) {
  return ts_subtree_is_inline(self)
    ? 0
    : !ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_NAMED) &&
      !ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_VISIBLE) &&
      self.ptr->child_count != 0;
}

static inline uint32_t ts_subtree_visible_descendant_count(Subtree self) {
  return (ts_subtree_is_inline(self) || self.ptr->child_count == 0)
    ? 0
    : self.ptr->visible_descendant_count;
}

static inline uint32_t ts_subtree_visible_child_count(Subtree self) {
  return ts_subtree_child_count(self) > 0 ? self.ptr->visible_child_count : 0;
}

static inline uint32_t ts_subtree__error_extent_cost(Length size) {
  return
    ERROR_COST_PER_RECOVERY +
    ERROR_COST_PER_SKIPPED_CHAR * size.bytes +
    ERROR_COST_PER_SKIPPED_LINE * size.extent.row;
}

static inline uint32_t ts_subtree_error_cost(Subtree self) {
  if (ts_subtree_missing(self)) {
    return ERROR_COST_PER_MISSING_TREE + ERROR_COST_PER_RECOVERY;
  }
  return ts_subtree_is_inline(self) ? 0 : self.ptr->error_cost;
}

static inline bool ts_subtree_has_completed_prefix(Subtree self) {
  if (ts_subtree_child_count(self) == 0) return false;
  return ts_subtree_error_cost(self) == 0 || (
    !ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_VISIBLE) &&
    ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_COMPLETED_PREFIX)
  );
}

static inline int32_t ts_subtree_dynamic_precedence(Subtree self) {
  if (ts_subtree_child_count(self) == 0 || ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_EXTRA_GROUP)) {
    return 0;
  }
  return self.ptr->dynamic_precedence;
}

static inline uint16_t ts_subtree_production_id(Subtree self) {
  if (ts_subtree_child_count(self) > 0 && !ts_subtree_heap_flag(self.ptr, SUBTREE_FLAG_EXTRA_GROUP)) {
    return self.ptr->production_id;
  }
  return 0;
}

static inline bool ts_subtree_is_error(Subtree self) {
  return ts_subtree_symbol(self) == ts_builtin_sym_error;
}

static inline bool ts_subtree_is_eof(Subtree self) {
  return ts_subtree_symbol(self) == ts_builtin_sym_end;
}

static inline Subtree ts_subtree_from_mut(MutableSubtree self) {
  return (Subtree) {.bits = self.bits};
}

static inline MutableSubtree ts_subtree_to_mut_unsafe(Subtree self) {
  return (MutableSubtree) {.bits = self.bits};
}

#ifdef __cplusplus
}
#endif

#endif  // TREE_SITTER_SUBTREE_H_
