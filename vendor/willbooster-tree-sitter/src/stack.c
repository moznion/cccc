// code-gauge-allow-file fileNcss -- GLR graph-structured stack operations share private node and iterator types
#include "./alloc.h"
#include "./language.h"
#include "./subtree.h"
#include "./array.h"
#include "./stack.h"
#include "./length.h"
#include <assert.h>
#include <inttypes.h>
#include <stdio.h>

#define MAX_LINK_COUNT 8
#define MAX_NODE_POOL_SIZE 256
#define MAX_ITERATOR_COUNT 64
#define MAX_EXTRA_CHAIN_COUNT 64

#if defined _WIN32 && !defined __GNUC__
#define forceinline __forceinline
#else
#define forceinline static inline __attribute__((always_inline))
#endif

typedef struct StackNode StackNode;

typedef struct {
  StackNode *node;
  Subtree subtree;
} StackLink;

// Fits within a single 64-byte cache line on 64-bit targets.
struct StackNode {
  Length position;
  uint32_t ref_count;
  unsigned error_cost;
  unsigned node_count;
  int dynamic_precedence;
  TSStateId state;
  uint8_t link_count;
  bool after_extra;
  union {
    StackLink inline_links[2];
    StackLink *overflow_links;
  };
};

#if TS_PTR_SIZE == 64
_Static_assert(sizeof(StackNode) == 64, "StackNode must fit in a 64-byte cache line");
#endif

forceinline StackLink *stack_node_links(StackNode *self) {
  return self->link_count > 2 ? self->overflow_links : self->inline_links;
}

forceinline const StackLink *stack_node_links_const(const StackNode *self) {
  return self->link_count > 2 ? self->overflow_links : self->inline_links;
}

typedef struct {
  StackNode *node;
  SubtreeArray subtrees;
  uint32_t subtree_count;
  uint32_t remaining_steps;
  int precedence;
  int total_precedence;
  uint32_t trimmed_count;
  uint32_t error_cost;
  uint32_t total_error_cost;
  uint32_t path;
} StackIterator;

typedef Array(StackNode *) StackNodeArray;

typedef struct {
  StackNode *start;
  StackNode *end;
} ExtraChain;

typedef enum {
  StackStatusActive,
  StackStatusPaused,
  StackStatusHalted,
} StackStatus;

typedef struct {
  StackNode *node;
  StackSummary *summary;
  unsigned node_count_at_last_error;
  uint32_t scanner_state_id;
  LookaheadToken lookahead_when_paused;
  StackStatus status;
  bool has_unconfirmed_missing_token;
  uint64_t unconfirmed_recovery;
  unsigned node_count_at_recovery;
} StackHead;

typedef struct {
  Subtree subtree;
  uint32_t previous;
} ReductionPath;
typedef Array(ReductionPath) ReductionPaths;
typedef struct {
  StackNode *node;
  uint32_t first;
  uint32_t last;
  int priority;
  uint32_t error_cost;
  bool selected;
} ReductionChoice;
typedef struct {
  uint32_t path;
  uint32_t length;
  int precedence;
  uint32_t error_cost;
  uint32_t next;
} ReductionCandidate;
typedef Array(ReductionCandidate) ReductionCandidates;
typedef Array(ReductionChoice) ReductionChoices;
typedef Array(uint32_t) ReductionSlots;

struct Stack {
  bool has_ambiguous_extras;
  Array(StackHead) heads;
  StackSliceArray slices;
  Array(StackIterator) iterators;
  Array(StackIterator) pop_all_iterators;
  StackSliceArray pop_all_slices;
  bool pop_all_started;
  StackNodeArray node_pool;
  StackNode *base_node;
  SubtreePool *subtree_pool;
  uint64_t recovery_generation;
  Array(ExtraChain) extra_chains;
};

typedef unsigned StackAction;
enum {
  StackActionNone,
  StackActionStop = 1,
  StackActionPop = 2,
};

typedef StackAction (*StackCallback)(void *, const StackIterator *);

static void stack_node_retain(StackNode *self) {
  if (!self)
    return;
  ts_assert(self->ref_count > 0);
  self->ref_count++;
  ts_assert(self->ref_count != 0);
}

static void stack_node_release(
  StackNode *self,
  StackNodeArray *pool,
  SubtreePool *subtree_pool
) {
  StackNodeArray pending = array_new();
recur:
  ts_assert(self->ref_count != 0);
  if (--self->ref_count > 0) goto next;

  StackLink *links = stack_node_links(self);
  StackNode *first_predecessor = NULL;
  for (unsigned i = self->link_count; i > 0; i--) {
    StackLink link = links[i - 1];
    if (link.subtree.ptr) ts_subtree_release(subtree_pool, link.subtree);
    if (i == 1) {
      first_predecessor = link.node;
    } else {
      array_push(&pending, link.node);
    }
  }
  if (self->link_count > 2) ts_free(self->overflow_links);
  if (pool->size < MAX_NODE_POOL_SIZE) {
    array_push(pool, self);
  } else {
    ts_free(self);
  }
  if (first_predecessor) {
    self = first_predecessor;
    goto recur;
  }

next:
  if (pending.size > 0) {
    self = array_pop(&pending);
    goto recur;
  }
  array_delete(&pending);
}

/// Get the number of nodes in the subtree, for the purpose of measuring
/// how much progress has been made by a given version of the stack.
static uint32_t stack__subtree_node_count(Subtree subtree) {
  uint32_t count = ts_subtree_visible_descendant_count(subtree);
  if (ts_subtree_visible(subtree)) count++;

  // Count intermediate error nodes even though they are not visible,
  // because a stack version's node count is used to check whether it
  // has made any progress since the last time it encountered an error.
  if (ts_subtree_symbol(subtree) == ts_builtin_sym_error_repeat && !ts_subtree_extra(subtree)) count++;

  return count;
}

static StackNode *stack_node_new(
  StackNode *previous_node,
  Subtree subtree,
  TSStateId state,
  StackNodeArray *pool
) {
  StackNode *node = pool->size > 0 ? array_pop(pool) : ts_malloc(sizeof(StackNode));
  if (!previous_node) {
    memset(node, 0, offsetof(StackNode, inline_links));
    node->ref_count = 1;
    node->state = state;
    return node;
  }

  bool is_extra = subtree.ptr && ts_subtree_extra(subtree);
  node->position = previous_node->position;
  node->ref_count = 1;
  node->error_cost = previous_node->error_cost;
  node->node_count = previous_node->node_count;
  node->dynamic_precedence = previous_node->dynamic_precedence;
  node->state = state;
  node->link_count = 1;
  node->after_extra = previous_node->after_extra;
  node->inline_links[0] = (StackLink) {.node = previous_node, .subtree = subtree};

  if (subtree.ptr) {
    if (is_extra || ts_subtree_total_bytes(subtree) || ts_subtree_has_external_tokens(subtree)) {
      node->after_extra = ts_subtree_trailing_extra(subtree);
    }
    node->error_cost += ts_subtree_error_cost(subtree);
    node->position = length_add(node->position, ts_subtree_total_size(subtree));
    node->node_count += stack__subtree_node_count(subtree);
    node->dynamic_precedence += ts_subtree_dynamic_precedence(subtree);
  }
  return node;
}

static bool stack__subtree_is_equivalent(Subtree left, Subtree right) {
  if (left.bits == right.bits) return true;
  if (!left.ptr || !right.ptr) return false;

  if (ts_subtree_symbol(left) != ts_subtree_symbol(right)) return false;

  if (ts_subtree_error_cost(left) > 0 && ts_subtree_error_cost(right) > 0) return true;

  return (
    ts_subtree_padding(left).bytes == ts_subtree_padding(right).bytes &&
    ts_subtree_size(left).bytes == ts_subtree_size(right).bytes &&
    ts_subtree_child_count(left) == ts_subtree_child_count(right) &&
    ts_subtree_extra(left) == ts_subtree_extra(right) &&
    ts_subtree_trailing_extra(left) == ts_subtree_trailing_extra(right) &&
    ts_subtree_scanner_state_id(left) == ts_subtree_scanner_state_id(right)
  );
}

static void stack__invalidate_extra_chains(Stack *stack, const StackNode *node) {
  // A merge can change an ancestor shared by several stack versions.
  for (uint32_t i = 0; i < stack->extra_chains.size; i++) {
    ExtraChain chain = *array_get(&stack->extra_chains, i);
    if (node->position.bytes >= chain.end->position.bytes &&
        node->position.bytes <= chain.start->position.bytes) {
      stack_node_release(chain.start, &stack->node_pool, stack->subtree_pool);
      array_erase(&stack->extra_chains, i--);
    }
  }
}

static void stack_node_update_node_count(StackNode *self);
static void stack_node_expand_extra_groups(Stack *, StackNode *);
static bool stack_node__try_merge_link(
  Stack *stack, StackNode *self, StackLink *existing_link, StackLink link, SubtreePool *subtree_pool
);
static bool stack_node__can_merge_predecessors(const StackNode *left, const StackNode *right);

static void stack_node_add_link(
  Stack *stack,
  StackNode *self,
  StackLink link,
  SubtreePool *subtree_pool
) {
  if (link.node == self) return;
  stack_node_expand_extra_groups(stack, self);

  StackLink *links = stack_node_links(self);
  for (unsigned i = 0; i < self->link_count; i++) {
    if (stack_node__try_merge_link(stack, self, &links[i], link, subtree_pool)) return;
  }

  if (self->link_count == MAX_LINK_COUNT) return;

  stack_node_retain(link.node);
  unsigned node_count = link.node->node_count;
  int dynamic_precedence = link.node->dynamic_precedence;
  stack__invalidate_extra_chains(stack, self);
  if (self->link_count == 2) {
    StackLink *overflow = ts_malloc(MAX_LINK_COUNT * sizeof(StackLink));
    overflow[0] = self->inline_links[0];
    overflow[1] = self->inline_links[1];
    self->overflow_links = overflow;
    links = overflow;
  }
  unsigned index = self->link_count++;
  links[index] = link;

  if (link.subtree.ptr) {
    ts_subtree_retain(link.subtree);
    node_count += stack__subtree_node_count(link.subtree);
    dynamic_precedence += ts_subtree_dynamic_precedence(link.subtree);
  }

  if (node_count > self->node_count) self->node_count = node_count;
  if (dynamic_precedence > self->dynamic_precedence) self->dynamic_precedence = dynamic_precedence;
}

static bool stack_node__try_merge_link(
  Stack *stack,
  StackNode *self,
  StackLink *existing_link,
  StackLink link,
  SubtreePool *subtree_pool
) {
  if (!stack__subtree_is_equivalent(existing_link->subtree, link.subtree)) return false;
  bool same_predecessor = existing_link->node == link.node;
  if (!same_predecessor && !stack_node__can_merge_predecessors(existing_link->node, link.node)) {
    return false;
  }

  bool subtree_changed = link.subtree.ptr &&
    ts_subtree_dynamic_precedence(link.subtree) >
    ts_subtree_dynamic_precedence(existing_link->subtree);
  if (subtree_changed) {
    ts_subtree_retain(link.subtree);
    ts_subtree_release(subtree_pool, existing_link->subtree);
    stack__invalidate_extra_chains(stack, self);
    existing_link->subtree = link.subtree;
  }
  if (!same_predecessor) {
    stack_node_expand_extra_groups(stack, link.node);
    const StackLink *pred_links = stack_node_links_const(link.node);
    for (unsigned j = 0; j < link.node->link_count; j++) {
      stack_node_add_link(stack, existing_link->node, pred_links[j], subtree_pool);
    }
  }
  if (subtree_changed || !same_predecessor) stack_node_update_node_count(self);
  int32_t dynamic_precedence = existing_link->node->dynamic_precedence +
    (existing_link->subtree.ptr ? ts_subtree_dynamic_precedence(existing_link->subtree) : 0);
  if (dynamic_precedence > self->dynamic_precedence) {
    self->dynamic_precedence = dynamic_precedence;
  }
  return true;
}

static bool stack_node__can_merge_predecessors(const StackNode *left, const StackNode *right) {
  return left->state == right->state &&
    left->after_extra == right->after_extra &&
    left->position.bytes == right->position.bytes &&
    left->error_cost == right->error_cost;
}

static void stack_node_update_node_count(StackNode *self) {
  self->node_count = 0;
  const StackLink *links = stack_node_links_const(self);
  for (unsigned i = 0; i < self->link_count; i++) {
    StackLink link = links[i];
    unsigned node_count = link.node->node_count;
    if (link.subtree.ptr) node_count += stack__subtree_node_count(link.subtree);
    if (node_count > self->node_count) self->node_count = node_count;
  }
}

static void stack_head_delete(
  StackHead *self,
  StackNodeArray *pool,
  SubtreePool *subtree_pool
) {
  if (self->node) {
    if (self->lookahead_when_paused.subtree.ptr) {
      ts_subtree_release(subtree_pool, self->lookahead_when_paused.subtree);
    }
    if (self->summary) {
      array_delete(self->summary);
      ts_free(self->summary);
    }
    stack_node_release(self->node, pool, subtree_pool);
  }
}

static StackVersion ts_stack__add_version(
  Stack *self,
  StackVersion original_version,
  StackNode *node
) {
  const StackHead *original = array_get(&self->heads, original_version);
  StackHead head = {
    .node = node,
    .node_count_at_last_error = original->node_count_at_last_error,
    .scanner_state_id = original->scanner_state_id,
    .status = StackStatusActive,
    .lookahead_when_paused = NULL_LOOKAHEAD,
    .has_unconfirmed_missing_token = original->has_unconfirmed_missing_token,
    .unconfirmed_recovery = original->unconfirmed_recovery,
    .node_count_at_recovery = original->node_count_at_recovery,
  };
  array_push(&self->heads, head);
  stack_node_retain(node);
  return (StackVersion)(self->heads.size - 1);
}

static void ts_stack__add_slice(
  Stack *self,
  StackVersion original_version,
  StackNode *node,
  SubtreeArray *subtrees
) {
  for (uint32_t i = self->slices.size - 1; i + 1 > 0; i--) {
    StackVersion version = array_get(&self->slices, i)->version;
    if (array_get(&self->heads, version)->node == node) {
      StackSlice slice = {*subtrees, version};
      array_insert(&self->slices, i + 1, slice);
      return;
    }
  }

  StackVersion version = ts_stack__add_version(self, original_version, node);
  StackSlice slice = { *subtrees, version };
  array_push(&self->slices, slice);
}

// code-gauge-allow functionCognitiveComplexity -- walks single-link extra chains while consulting and updating the bounded LRU cache
static StackNode *stack__skip_extra_chains(Stack *self, StackNode *node) {
  StackNode *start = node;
  uint32_t cached_index = UINT32_MAX;
  unsigned steps = 0;
  while (node->link_count == 1 && node->inline_links[0].subtree.ptr &&
         ts_subtree_extra(node->inline_links[0].subtree) && node->state == node->inline_links[0].node->state) {
    for (uint32_t i = 0; i < self->extra_chains.size; i++) {
      ExtraChain chain = *array_get(&self->extra_chains, i);
      if (node == chain.start) {
        cached_index = i;
        node = chain.end;
        break;
      }
    }
    if (cached_index != UINT32_MAX) break;
    node = node->inline_links[0].node;
    steps++;
  }
  // Single links do not save traversal and can evict useful directive chains.
  if (steps > 1 || cached_index != UINT32_MAX) {
    stack_node_retain(start);
    if (cached_index == UINT32_MAX && self->extra_chains.size == MAX_EXTRA_CHAIN_COUNT) {
      cached_index = 0;
    }
    if (cached_index != UINT32_MAX) {
      stack_node_release(array_get(&self->extra_chains, cached_index)->start, &self->node_pool, self->subtree_pool);
      array_erase(&self->extra_chains, cached_index);
    }
    array_push(&self->extra_chains, ((ExtraChain) {.start = start, .end = node}));
  }
  return node;
}

// code-gauge-allow functionCognitiveComplexity -- open-addressed hash table resize and candidate grouping stay inline on the hot pop path
static void stack__record_reduction(
  ReductionChoices *groups,
  ReductionCandidates *candidates,
  const StackIterator *iterator,
  ReductionSlots *slots
) {
  int priority = iterator->node->dynamic_precedence + iterator->total_precedence;
  uint32_t error_cost = iterator->node->error_cost + iterator->total_error_cost;
  uint32_t candidate = candidates->size;
  array_push(candidates, ((ReductionCandidate) {
    iterator->path, iterator->trimmed_count, iterator->precedence, iterator->error_cost, UINT32_MAX
  }));
  if (groups->size * 2 >= slots->size) {
    uint32_t capacity = slots->size ? slots->size * 2 : 16;
    array_grow_by(slots, capacity - slots->size);
    memset(slots->contents, 0, capacity * sizeof(uint32_t));
    for (uint32_t i = 0; i < groups->size; i++) {
      uint32_t slot = ((uintptr_t)array_get(groups, i)->node >> 4) & (capacity - 1);
      while (*array_get(slots, slot)) slot = (slot + 1) & (capacity - 1);
      *array_get(slots, slot) = i + 1;
    }
  }
  uint32_t slot = ((uintptr_t)iterator->node >> 4) & (slots->size - 1);
  while (*array_get(slots, slot)) {
    ReductionChoice *group = array_get(groups, *array_get(slots, slot) - 1);
    if (group->node == iterator->node) {
      array_get(candidates, group->last)->next = candidate;
      group->last = candidate;
      if (error_cost < group->error_cost) {
        group->error_cost = error_cost;
        group->priority = priority;
      } else if (error_cost == group->error_cost && priority > group->priority) {
        group->priority = priority;
      }
      return;
    }
    slot = (slot + 1) & (slots->size - 1);
  }
  array_push(groups, ((ReductionChoice) {iterator->node, candidate, candidate, priority, error_cost, false}));
  *array_get(slots, slot) = groups->size;
}

static SubtreeArray stack__materialize_reduction(
  Stack *, const ReductionPaths *, const ReductionCandidates *, const ReductionChoice *
);

static void stack__skip_idle_steps(Stack *);

typedef struct {
  StackProgressCallback callback;
  void *payload;
  bool canceled;
} StackIterationProgress;

// code-gauge-allow functionParameterCount -- traversal configuration and optional reduction and progress hooks
static StackSliceArray stack__iter(
  Stack *self,
  StackVersion version,
  StackCallback callback,
  void *payload,
  int goal_subtree_count,
  uint32_t max_reductions,
  StackReductionStatus *reduction_status,
  StackIterationProgress *progress
) {
  StackHead *head = array_get(&self->heads, version);
  bool compact = reduction_status && self->has_ambiguous_extras;
  // The original head pins every borrowed path until the selected arrays retain it.
  uint32_t pop_count = 0;
  bool invalid_path = false;
  ReductionPaths paths = array_new();
  ReductionChoices winners = array_new();
  ReductionCandidates candidates = array_new();
  ReductionSlots slots = array_new();
  bool include_subtrees = goal_subtree_count >= 0;
  bool stop_after_error_path = progress && head->node->error_cost > 0;
  if (!progress || !self->pop_all_started) {
    array_clear(&self->slices);
    array_clear(&self->iterators);
    StackIterator new_iterator = {
      .node = head->node,
      .subtrees = array_new(),
      .subtree_count = 0,
      .path = UINT32_MAX,
    };
    if (include_subtrees && !compact && goal_subtree_count > 0) {
      array_reserve(&new_iterator.subtrees, (uint32_t)goal_subtree_count);
    }
    array_push(&self->iterators, new_iterator);
  }

  while (self->iterators.size > 0) {
    if (progress && progress->callback && !progress->callback(progress->payload, self->iterators.size)) {
      ts_assert(!compact);
      progress->canceled = true;
      break;
    }
    stack__skip_idle_steps(self);
    // Coalescing bounds summary-only traversal. The order-dependent summary callback
    // can change candidate selection when duplicate visits are removed.
    if (!include_subtrees) {
      for (uint32_t i = 0; i < self->iterators.size; i++) {
        for (uint32_t j = i + 1; j < self->iterators.size; j++) {
          StackIterator *left = array_get(&self->iterators, i);
          StackIterator *right = array_get(&self->iterators, j);
          if (left->node == right->node && left->subtree_count == right->subtree_count && left->remaining_steps == right->remaining_steps) {
            array_delete(&right->subtrees);
            array_erase(&self->iterators, j--);
          }
        }
      }
    }
    for (uint32_t i = 0, size = self->iterators.size; i < size; i++) {
      StackIterator *iterator = array_get(&self->iterators, i);
      if (iterator->remaining_steps) {
        iterator->remaining_steps--;
        continue;
      }
      StackNode *node = iterator->node;

      StackAction action = callback(payload, iterator);
      // Keep competing paths in breadth-first order: their first position wins.
      if (!include_subtrees && self->iterators.size == 1 && action == StackActionNone) {
        node = stack__skip_extra_chains(self, node);
        iterator->node = node;
      }
      bool should_pop = action & StackActionPop;
      bool should_stop = action & StackActionStop || node->link_count == 0;

      if (should_pop) pop_count++;
      if (should_pop && compact) {
        stack__record_reduction(&winners, &candidates, iterator, &slots);
      } else if (should_pop) {
        SubtreeArray subtrees = iterator->subtrees;
        if (!should_stop) {
          ts_subtree_array_copy(subtrees, &subtrees);
        }
        ts_subtree_array_reverse(&subtrees);
        ts_stack__add_slice(
          self,
          version,
          node,
          &subtrees
        );
      }

      if (should_stop) {
        if (!should_pop) {
          ts_subtree_array_delete(self->subtree_pool, &iterator->subtrees);
        }
        array_erase(&self->iterators, i);
        i--, size--;
        continue;
      }

      const StackLink *links = stack_node_links_const(node);
      for (uint32_t j = 1; j <= node->link_count; j++) {
        StackIterator *next_iterator;
        uint32_t link_index = j == node->link_count ? 0 : j;
        StackLink link = links[link_index];
        if (j == node->link_count) {
          next_iterator = array_get(&self->iterators, i);
        } else {
          if (self->iterators.size >= MAX_ITERATOR_COUNT) continue;
          StackIterator current_iterator = *array_get(&self->iterators, i);
          array_push(&self->iterators, current_iterator);
          next_iterator = array_back(&self->iterators);
          if (!compact) ts_subtree_array_copy(next_iterator->subtrees, &next_iterator->subtrees);
        }

        next_iterator->node = link.node;
        next_iterator->remaining_steps = ts_subtree_stack_link_count(link.subtree) - 1;
        if (link.subtree.ptr) {
          if (include_subtrees) {
            if (compact) {
              next_iterator->total_precedence += ts_subtree_dynamic_precedence(link.subtree);
              next_iterator->total_error_cost += ts_subtree_error_cost(link.subtree);
            }
            if (compact && (next_iterator->subtree_count > 0 || !ts_subtree_extra(link.subtree))) {
              next_iterator->precedence += ts_subtree_dynamic_precedence(link.subtree);
              uint32_t error_cost = ts_subtree_error_cost(link.subtree);
              if (ts_subtree_symbol(link.subtree) == ts_builtin_sym_error_repeat && !ts_subtree_extra(link.subtree)) {
                uint32_t extent_cost = ts_subtree__error_extent_cost(ts_subtree_size(link.subtree));
                error_cost -= extent_cost;
                next_iterator->total_error_cost -= extent_cost;
              }
              next_iterator->error_cost += error_cost;
              next_iterator->trimmed_count++;
            }
            if (compact) {
              array_push(&paths, ((ReductionPath) {link.subtree, next_iterator->path}));
              next_iterator->path = paths.size - 1;
            } else {
              array_push(&next_iterator->subtrees, link.subtree);
              ts_subtree_retain(link.subtree);
            }
          }

          if (!ts_subtree_extra(link.subtree)) {
            next_iterator->subtree_count++;
          }
        } else {
          if (compact) invalid_path = true;
          next_iterator->subtree_count++;
        }
      }
    }
    if (stop_after_error_path && self->slices.size > 0) {
      // Every path in this accepted error version is invalid; retaining all of them can use quadratic memory.
      for (uint32_t i = 0; i < self->iterators.size; i++) {
        ts_subtree_array_delete(self->subtree_pool, &array_get(&self->iterators, i)->subtrees);
      }
      array_clear(&self->iterators);
      break;
    }
  }

  if (compact && !invalid_path) {
    reduction_status->was_truncated = winners.size > max_reductions;
    for (uint32_t selected = 0; selected < max_reductions; selected++) {
      uint32_t best = UINT32_MAX;
      for (uint32_t i = 0; i < winners.size; i++) {
        ReductionChoice *group = array_get(&winners, i);
        if (!group->selected && (best == UINT32_MAX ||
            group->error_cost < array_get(&winners, best)->error_cost ||
            (group->error_cost == array_get(&winners, best)->error_cost &&
             group->priority > array_get(&winners, best)->priority))) {
          best = i;
        }
      }
      if (best == UINT32_MAX) break;
      array_get(&winners, best)->selected = true;
    }
    uint32_t retained = 0;
    for (uint32_t i = 0; i < winners.size; i++) {
      if (array_get(&winners, i)->selected) {
        *array_get(&winners, retained) = *array_get(&winners, i);
        retained++;
      }
    }
    winners.size = retained;
    for (uint32_t i = 0; i < winners.size; i++) {
      ReductionChoice group = *array_get(&winners, i);
      SubtreeArray subtrees = stack__materialize_reduction(self, &paths, &candidates, &group);
      StackVersion slice_version = ts_stack__add_version(self, version, group.node);
      array_push(&self->slices, ((StackSlice) {subtrees, slice_version}));
    }
  }

  array_delete(&slots);
  array_delete(&paths);
  array_delete(&winners);
  array_delete(&candidates);
  if (reduction_status) reduction_status->has_multiple_paths = pop_count > 1;
  if (compact && invalid_path) {
    StackSliceArray result = stack__iter(self, version, callback, payload, goal_subtree_count, 0, NULL, NULL);
    reduction_status->has_multiple_paths = result.size > 1;
    return result;
  }
  return self->slices;
}

// code-gauge-allow functionCognitiveComplexity -- compares reduction candidates by error cost, precedence, length, and subtree order
// code-gauge-allow functionNcss -- candidate winner selection and reversed path materialization share local path indices
static SubtreeArray stack__materialize_reduction(
  Stack *self,
  const ReductionPaths *paths,
  const ReductionCandidates *candidates,
  const ReductionChoice *group
) {
  SubtreeArray subtrees = array_new();
  uint32_t first = group->first;
  ReductionCandidate winner = *array_get(candidates, first);
  for (uint32_t i = winner.next; i != UINT32_MAX;) {
    ReductionCandidate candidate = *array_get(candidates, i);
    bool replace;
    if (candidate.error_cost != winner.error_cost) {
      replace = candidate.error_cost < winner.error_cost;
    } else if (candidate.precedence != winner.precedence) {
      replace = candidate.precedence > winner.precedence;
    } else if (candidate.error_cost > 0) {
      replace = true;
    } else if (candidate.length != winner.length) {
      replace = candidate.length < winner.length;
    } else {
      uint32_t left = winner.path, right = candidate.path;
      int comparison = 0;
      for (uint32_t j = 0; j < winner.length && !comparison; j++) {
        ReductionPath a = *array_get(paths, left);
        ReductionPath b = *array_get(paths, right);
        if (a.subtree.bits != b.subtree.bits) {
          comparison = ts_subtree_compare(a.subtree, b.subtree, self->subtree_pool);
        }
        left = a.previous;
        right = b.previous;
      }
      replace = comparison > 0;
    }
    if (replace) winner = candidate;
    i = candidate.next;
  }
  uint32_t path_index = winner.path;
  while (path_index != UINT32_MAX) {
    ReductionPath path = *array_get(paths, path_index);
    array_push(&subtrees, path.subtree);
    ts_subtree_retain(path.subtree);
    path_index = path.previous;
  }
  return subtrees;
}

static void stack__clear_pop_all(Stack *self) {
  for (uint32_t i = 0; i < self->pop_all_iterators.size; i++) {
    ts_subtree_array_delete(self->subtree_pool, &array_get(&self->pop_all_iterators, i)->subtrees);
  }
  array_clear(&self->pop_all_iterators);
  for (uint32_t i = 0; i < self->pop_all_slices.size; i++) {
    ts_subtree_array_delete(self->subtree_pool, &array_get(&self->pop_all_slices, i)->subtrees);
  }
  array_clear(&self->pop_all_slices);
  self->pop_all_started = false;
}

Stack *ts_stack_new(SubtreePool *subtree_pool) {
  Stack *self = ts_calloc(1, sizeof(Stack));
  array_reserve(&self->heads, 4);
  array_reserve(&self->slices, 4);
  array_reserve(&self->iterators, 4);
  array_reserve(&self->node_pool, MAX_NODE_POOL_SIZE);

  self->subtree_pool = subtree_pool;
  self->base_node = stack_node_new(NULL, NULL_SUBTREE, 1, &self->node_pool);
  ts_stack_clear(self);

  return self;
}

void ts_stack_delete(Stack *self) {
  stack__clear_pop_all(self);
  array_delete(&self->pop_all_iterators);
  array_delete(&self->pop_all_slices);
  for (uint32_t i = 0; i < self->extra_chains.size; i++) {
    stack_node_release(array_get(&self->extra_chains, i)->start, &self->node_pool, self->subtree_pool);
  }
  array_delete(&self->extra_chains);
  array_delete(&self->slices);
  array_delete(&self->iterators);
  stack_node_release(self->base_node, &self->node_pool, self->subtree_pool);
  for (uint32_t i = 0; i < self->heads.size; i++) {
    stack_head_delete(array_get(&self->heads, i), &self->node_pool, self->subtree_pool);
  }
  array_delete(&self->heads);
  for (uint32_t i = 0; i < self->node_pool.size; i++) {
    ts_free(*array_get(&self->node_pool, i));
  }
  array_delete(&self->node_pool);
  ts_free(self);
}

uint32_t ts_stack_version_count(const Stack *self) {
  return self->heads.size;
}

uint32_t ts_stack_halted_version_count(Stack *self) {
  uint32_t count = 0;
  for (uint32_t i = 0; i < self->heads.size; i++) {
    StackHead *head = array_get(&self->heads, i);
    if (head->status == StackStatusHalted) {
      count++;
    }
  }
  return count;
}

TSStateId ts_stack_state(const Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->node->state;
}

Length ts_stack_position(const Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->node->position;
}

bool ts_stack_after_extra(const Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->node->after_extra;
}

uint32_t ts_stack_external_scanner_state_id(const Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->scanner_state_id;
}

void ts_stack_set_external_scanner_state_id(Stack *self, StackVersion version, uint32_t state_id) {
  array_get(&self->heads, version)->scanner_state_id = state_id;
}

unsigned ts_stack_error_cost(const Stack *self, StackVersion version) {
  StackHead *head = array_get(&self->heads, version);
  unsigned result = head->node->error_cost;
  if (
    head->status == StackStatusPaused ||
    (head->node->state == ERROR_STATE &&
     (head->node->link_count == 0 || !stack_node_links_const(head->node)[0].subtree.ptr))) {
    result += ERROR_COST_PER_RECOVERY;
  }
  return result;
}

uint64_t ts_stack_unconfirmed_recovery(const Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->unconfirmed_recovery;
}

void ts_stack_set_unconfirmed_recovery(Stack *self, StackVersion version, bool value) {
  StackHead *head = array_get(&self->heads, version);
  head->unconfirmed_recovery = value ? ++self->recovery_generation : 0;
  head->node_count_at_recovery = head->node->node_count;
}

unsigned ts_stack_node_count_since_recovery(const Stack *self, StackVersion version) {
  const StackHead *head = array_get(&self->heads, version);
  return head->node->node_count > head->node_count_at_recovery
    ? head->node->node_count - head->node_count_at_recovery
    : 0;
}

bool ts_stack_has_unconfirmed_missing_token(const Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->has_unconfirmed_missing_token;
}

void ts_stack_set_unconfirmed_missing_token(Stack *self, StackVersion version, bool value) {
  array_get(&self->heads, version)->has_unconfirmed_missing_token = value;
}

unsigned ts_stack_node_count_since_error(const Stack *self, StackVersion version) {
  StackHead *head = array_get(&self->heads, version);
  if (head->node->node_count < head->node_count_at_last_error) {
    head->node_count_at_last_error = head->node->node_count;
  }
  return head->node->node_count - head->node_count_at_last_error;
}

void ts_stack_push_recovery(
  Stack *self, StackVersion version, Subtree subtree, TSStateId state,
  const TSLanguage *language
) {
  ts_assert(ts_subtree_is_error(subtree) && ts_subtree_extra(subtree));
  StackHead *head = array_get(&self->heads, version);
  StackNode *node = head->node;
  Subtree prefix = NULL_SUBTREE;
  unsigned extra_count = 0;
  while (node->state == state && node->link_count == 1 &&
         node->inline_links[0].node->state == state &&
         node->inline_links[0].subtree.ptr && ts_subtree_extra(node->inline_links[0].subtree)) {
    Subtree extra = node->inline_links[0].subtree;
    ts_subtree_retain(extra);
    prefix = prefix.ptr ? ts_subtree_new_extra_group(self->subtree_pool, extra, prefix, language) : extra;
    node = node->inline_links[0].node;
    extra_count++;
  }
  if (extra_count > 1) {
    StackLink old_link = head->node->inline_links[0];
    stack_node_retain(node);
    head->node->inline_links[0] = (StackLink) {.node = node, .subtree = prefix};
    ts_subtree_release(self->subtree_pool, old_link.subtree);
    stack_node_release(old_link.node, &self->node_pool, self->subtree_pool);
  } else if (prefix.ptr) {
    ts_subtree_release(self->subtree_pool, prefix);
  }
  ts_stack_push(self, version, subtree, state);
}

void ts_stack_push(
  Stack *self,
  StackVersion version,
  Subtree subtree,
  TSStateId state
) {
  StackHead *head = array_get(&self->heads, version);
  StackNode *new_node = stack_node_new(head->node, subtree, state, &self->node_pool);
  if (!subtree.ptr) head->node_count_at_last_error = new_node->node_count;
  head->node = new_node;
}

void ts_stack_record_ambiguous_extra(Stack *self) {
  self->has_ambiguous_extras = true;
}

forceinline StackAction pop_count_callback(void *payload, const StackIterator *iterator) {
  return iterator->subtree_count == *(unsigned *)payload
    ? StackActionPop | StackActionStop
    : StackActionNone;
}

// Pops `count` subtrees along the only path from the head, producing what `stack__iter` with
// `pop_count_callback` produces there. Returns false, changing nothing, when a node on the way has
// several links.
static bool stack__pop_count_on_single_path(Stack *self, StackVersion version, uint32_t count) {
  StackNode *head_node = array_get(&self->heads, version)->node;
  uint32_t link_steps = 0;
  uint32_t total_subtrees = 0;
  for (const StackNode *scan = head_node; count > 0; scan = scan->inline_links[0].node, link_steps++) {
    if (scan->link_count > 1) return false;
    if (scan->link_count == 0) {
      array_clear(&self->slices);
      array_clear(&self->iterators);
      return true;
    }
    Subtree subtree = scan->inline_links[0].subtree;
    total_subtrees += subtree.ptr != NULL;
    count -= !subtree.ptr || !ts_subtree_extra(subtree);
  }

  array_clear(&self->slices);
  array_clear(&self->iterators);
  SubtreeArray subtrees = array_new();
  if (total_subtrees > 0) {
    array_reserve(&subtrees, total_subtrees);
    subtrees.size = total_subtrees;
  }
  StackNode *node = head_node;
  for (uint32_t i = total_subtrees; link_steps > 0; link_steps--, node = node->inline_links[0].node) {
    Subtree subtree = node->inline_links[0].subtree;
    if (!subtree.ptr) continue;
    subtrees.contents[--i] = subtree;
    ts_subtree_retain(subtree);
  }
  ts_stack__add_slice(self, version, node, &subtrees);
  return true;
}

StackSliceArray ts_stack_pop_count(Stack *self, StackVersion version, uint32_t count) {
  if (stack__pop_count_on_single_path(self, version, count)) return self->slices;
  return stack__iter(self, version, pop_count_callback, &count, (int)count, 0, NULL, NULL);
}

StackSliceArray ts_stack_pop_count_for_reduce(
  Stack *self,
  StackVersion version,
  uint32_t count,
  uint32_t max_reductions,
  StackReductionStatus *reduction_status
) {
  if (reduction_status) *reduction_status = (StackReductionStatus) {0};
  // On a single path, compacting ambiguous extras has only one candidate to select, so the result is the same.
  return stack__pop_count_on_single_path(self, version, count)
    ? self->slices
    : stack__iter(self, version, pop_count_callback, &count, (int)count, max_reductions, reduction_status, NULL);
}

static StackNode *stack__recycle_popped_links(Stack *self, StackNode *node, Subtree *dest, uint32_t count);

// Pops `count` non-extra subtrees in place when every popped StackNode is uniquely owned,
// writing children directly into a newly allocated TreeArena node buffer.
SubtreeHeapData *ts_stack_pop_count_in_place(
  Stack *self,
  StackVersion version,
  uint32_t count,
  uint32_t *out_child_count,
  SubtreeArray *trailing_extras
) {
  if (self->heads.size != 1 || self->pop_all_started) return NULL;
  StackHead *head = array_get(&self->heads, version);
  uint32_t trailing_count = 0;
  uint32_t child_count = 0;
  for (const StackNode *scan = head->node; count > 0; scan = scan->inline_links[0].node) {
    if (scan->link_count != 1 || scan->ref_count != 1) return NULL;
    Subtree subtree = scan->inline_links[0].subtree;
    if (!subtree.ptr) return NULL;
    bool is_extra = ts_subtree_extra(subtree);
    bool before_children = child_count == 0 && is_extra;
    trailing_count += before_children;
    child_count += !before_children;
    count -= !is_extra;
  }

  array_clear(trailing_extras);
  if (trailing_count > 0) {
    array_reserve(trailing_extras, trailing_count);
    trailing_extras->size = trailing_count;
  }

  SubtreeHeapData *data = ts_subtree_pool_alloc_node(self->subtree_pool, child_count);
  Subtree *children = (Subtree *)data - child_count;
  StackNode *node = stack__recycle_popped_links(self, head->node, trailing_extras->contents, trailing_count);
  head->node = stack__recycle_popped_links(self, node, children, child_count);
  *out_child_count = child_count;
  return data;
}

static StackNode *stack__recycle_popped_links(
  Stack *self,
  StackNode *node,
  Subtree *dest,
  uint32_t count
) {
  for (uint32_t i = count; i > 0; i--) {
    StackLink link = node->inline_links[0];
    dest[i - 1] = link.subtree;
    if (self->node_pool.size < MAX_NODE_POOL_SIZE) {
      self->node_pool.contents[self->node_pool.size++] = node;
    } else {
      ts_free(node);
    }
    node = link.node;
  }
  return node;
}

forceinline StackAction pop_error_callback(void *payload, const StackIterator *iterator) {
  if (iterator->subtrees.size == 0) return StackActionNone;
  bool *found_error = payload;
  if (*found_error || !ts_subtree_is_error(*array_get(&iterator->subtrees, 0))) return StackActionStop;
  *found_error = true;
  return StackActionPop | StackActionStop;
}

SubtreeArray ts_stack_pop_error(Stack *self, StackVersion version) {
  StackNode *node = array_get(&self->heads, version)->node;
  stack_node_expand_extra_groups(self, node);
  const StackLink *links = stack_node_links_const(node);
  for (unsigned i = 0; i < node->link_count; i++) {
    if (links[i].subtree.ptr && ts_subtree_is_error(links[i].subtree)) {
      bool found_error = false;
      StackSliceArray pop = stack__iter(self, version, pop_error_callback, &found_error, 1, 0, NULL, NULL);
      if (pop.size > 0) {
        ts_assert(pop.size == 1);
        ts_stack_renumber_version(self, array_get(&pop, 0)->version, version);
        return array_get(&pop, 0)->subtrees;
      }
      break;
    }
  }
  return (SubtreeArray) {.size = 0};
}

forceinline StackAction pop_all_callback(void *payload, const StackIterator *iterator) {
  (void)payload;
  return iterator->node->link_count == 0 ? StackActionPop : StackActionNone;
}

bool ts_stack_pop_all(
  Stack *self,
  StackVersion version,
  StackSliceArray *result,
  StackProgressCallback callback,
  void *payload
) {
  array_swap(&self->iterators, &self->pop_all_iterators);
  array_swap(&self->slices, &self->pop_all_slices);
  StackIterationProgress progress = {
    .callback = callback,
    .payload = payload,
  };
  *result = stack__iter(self, version, pop_all_callback, NULL, 0, 0, NULL, &progress);
  self->pop_all_started = progress.canceled;
  array_swap(&self->iterators, &self->pop_all_iterators);
  array_swap(&self->slices, &self->pop_all_slices);
  return !progress.canceled;
}

typedef struct {
  Stack *stack;
  StackSummary *summary;
  unsigned max_depth;
} SummarizeStackSession;

forceinline StackAction summarize_stack_callback(void *payload, const StackIterator *iterator) {
  SummarizeStackSession *session = payload;
  TSStateId state = iterator->node->state;
  unsigned depth = iterator->subtree_count;
  if (depth > session->max_depth) return StackActionStop;
  for (unsigned i = session->summary->size - 1; i + 1 > 0; i--) {
    StackSummaryEntry entry = *array_get(session->summary, i);
    if (entry.depth < depth) break;
    if (entry.depth == depth && entry.state == state) return StackActionNone;
  }
  bool has_root_subtree = false;
  StackNode *node = stack__skip_extra_chains(session->stack, iterator->node);
  const StackLink *links = stack_node_links_const(node);
  for (unsigned i = 0; i < node->link_count; i++) {
    StackLink link = links[i];
    if (link.subtree.ptr && link.node->state == 1 &&
        ts_subtree_has_completed_prefix(link.subtree)) {
      has_root_subtree = true;
    }
  }
  array_push(session->summary, ((StackSummaryEntry) {
    .position = iterator->node->position,
    .depth = depth,
    .state = state,
    .has_root_subtree = has_root_subtree,
  }));
  return StackActionNone;
}

void ts_stack_record_summary(Stack *self, StackVersion version, unsigned max_depth) {
  StackSummary summary = ts_stack_current_summary(self, version, max_depth);
  StackHead *head = array_get(&self->heads, version);
  if (head->summary) {
    array_delete(head->summary);
  } else {
    head->summary = ts_malloc(sizeof(StackSummary));
  }
  *head->summary = summary;
}

StackSummary ts_stack_current_summary(Stack *self, StackVersion version, unsigned max_depth) {
  StackSummary summary = array_new();
  SummarizeStackSession session = {
    .stack = self,
    .summary = &summary,
    .max_depth = max_depth
  };
  stack__iter(self, version, summarize_stack_callback, &session, -1, 0, NULL, NULL);
  return summary;
}

StackSummary *ts_stack_get_summary(Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->summary;
}

int ts_stack_dynamic_precedence(Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->node->dynamic_precedence;
}

bool ts_stack_has_advanced_since_error(const Stack *self, StackVersion version) {
  const StackHead *head = array_get(&self->heads, version);
  const StackNode *node = head->node;
  if (node->error_cost == 0) return true;
  while (node) {
    if (node->link_count > 0) {
      const StackLink *links = stack_node_links_const(node);
      Subtree subtree = links[0].subtree;
      if (subtree.ptr) {
        if (ts_subtree_total_bytes(subtree) > 0) {
          return true;
        } else if (
          node->node_count > head->node_count_at_last_error &&
          ts_subtree_error_cost(subtree) == 0
        ) {
          node = links[0].node;
          continue;
        }
      }
    }
    break;
  }
  return false;
}

void ts_stack_remove_version(Stack *self, StackVersion version) {
  stack_head_delete(array_get(&self->heads, version), &self->node_pool, self->subtree_pool);
  array_erase(&self->heads, version);
}

void ts_stack_renumber_version(Stack *self, StackVersion v1, StackVersion v2) {
  if (v1 == v2) return;
  ts_assert(v2 < v1);
  ts_assert((uint32_t)v1 < self->heads.size);
  StackHead *source_head = array_get(&self->heads, v1);
  StackHead *target_head = array_get(&self->heads, v2);
  if (target_head->summary && !source_head->summary) {
    source_head->summary = target_head->summary;
    target_head->summary = NULL;
  }
  stack_head_delete(target_head, &self->node_pool, self->subtree_pool);
  *target_head = *source_head;
  array_erase(&self->heads, v1);
}

void ts_stack_swap_versions(Stack *self, StackVersion v1, StackVersion v2) {
  StackHead temporary_head = *array_get(&self->heads, v1);
  *array_get(&self->heads, v1) = *array_get(&self->heads, v2);
  *array_get(&self->heads, v2) = temporary_head;
}

StackVersion ts_stack_copy_version(Stack *self, StackVersion version) {
  ts_assert(version < self->heads.size);
  StackHead version_head = *array_get(&self->heads, version);
  array_push(&self->heads, version_head);
  StackHead *head = array_back(&self->heads);
  stack_node_retain(head->node);
  head->summary = NULL;
  return self->heads.size - 1;
}

bool ts_stack_merge(Stack *self, StackVersion version1, StackVersion version2) {
  if (!ts_stack_can_merge(self, version1, version2)) return false;
  StackHead *head1 = array_get(&self->heads, version1);
  StackHead *head2 = array_get(&self->heads, version2);
  stack_node_expand_extra_groups(self, head1->node);
  stack_node_expand_extra_groups(self, head2->node);
  if (!head2->unconfirmed_recovery) head1->unconfirmed_recovery = 0;
  const StackLink *links2 = stack_node_links_const(head2->node);
  for (uint32_t i = 0; i < head2->node->link_count; i++) {
    stack_node_add_link(self, head1->node, links2[i], self->subtree_pool);
  }
  if (head1->node->state == ERROR_STATE) {
    head1->node_count_at_last_error = head1->node->node_count;
  }
  ts_stack_remove_version(self, version2);
  return true;
}

bool ts_stack_can_merge(Stack *self, StackVersion version1, StackVersion version2) {
  StackHead *head1 = array_get(&self->heads, version1);
  StackHead *head2 = array_get(&self->heads, version2);
  return
    head1->status == StackStatusActive &&
    head2->status == StackStatusActive &&
    head1->node->state == head2->node->state &&
    head1->node->after_extra == head2->node->after_extra &&
    head1->node->position.bytes == head2->node->position.bytes &&
    head1->node->error_cost == head2->node->error_cost &&
    head1->scanner_state_id == head2->scanner_state_id;
}

void ts_stack_halt(Stack *self, StackVersion version) {
  array_get(&self->heads, version)->status = StackStatusHalted;
}

void ts_stack_pause(Stack *self, StackVersion version, LookaheadToken lookahead) {
  StackHead *head = array_get(&self->heads, version);
  head->status = StackStatusPaused;
  head->lookahead_when_paused = lookahead;
  head->node_count_at_last_error = head->node->node_count;
}

bool ts_stack_is_active(const Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->status == StackStatusActive;
}

bool ts_stack_is_halted(const Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->status == StackStatusHalted;
}

bool ts_stack_is_paused(const Stack *self, StackVersion version) {
  return array_get(&self->heads, version)->status == StackStatusPaused;
}

LookaheadToken ts_stack_resume(Stack *self, StackVersion version) {
  StackHead *head = array_get(&self->heads, version);
  ts_assert(head->status == StackStatusPaused);
  LookaheadToken result = head->lookahead_when_paused;
  head->status = StackStatusActive;
  head->lookahead_when_paused = NULL_LOOKAHEAD;
  return result;
}

void ts_stack_clear(Stack *self) {
  stack__clear_pop_all(self);
  self->has_ambiguous_extras = false;
  for (uint32_t i = 0; i < self->extra_chains.size; i++) {
    stack_node_release(array_get(&self->extra_chains, i)->start, &self->node_pool, self->subtree_pool);
  }
  array_clear(&self->extra_chains);
  stack_node_retain(self->base_node);
  for (uint32_t i = 0; i < self->heads.size; i++) {
    stack_head_delete(array_get(&self->heads, i), &self->node_pool, self->subtree_pool);
  }
  array_clear(&self->heads);
  self->recovery_generation = 0;
  array_push(&self->heads, ((StackHead) {
    .node = self->base_node,
    .status = StackStatusActive,
    .scanner_state_id = 0,
    .lookahead_when_paused = NULL_LOOKAHEAD,
  }));
}

bool ts_stack_print_dot_graph(Stack *self, const TSLanguage *language, FILE *f) {
  array_reserve(&self->iterators, 32);
  if (!f) f = stderr;

  fprintf(f, "digraph stack {\n");
  fprintf(f, "rankdir=\"RL\";\n");
  fprintf(f, "edge [arrowhead=none]\n");

  Array(StackNode *) visited_nodes = array_new();

  array_clear(&self->iterators);
  for (uint32_t i = 0; i < self->heads.size; i++) {
    StackHead *head = array_get(&self->heads, i);
    if (head->status == StackStatusHalted) continue;

    fprintf(f, "node_head_%u [shape=none, label=\"\"]\n", i);
    fprintf(f, "node_head_%u -> node_%p [", i, (void *)head->node);

    if (head->status == StackStatusPaused) {
      fprintf(f, "color=red ");
    }
    fprintf(f,
      "label=%u, fontcolor=blue, weight=10000, labeltooltip=\"node_count: %u\nerror_cost: %u",
      i,
      ts_stack_node_count_since_error(self, i),
      ts_stack_error_cost(self, i)
    );

    if (head->summary) {
      fprintf(f, "\nsummary:");
      for (uint32_t j = 0; j < head->summary->size; j++) fprintf(f, " %u", array_get(head->summary, j)->state);
    }

    if (head->scanner_state_id != 0) {
      fprintf(f, "\nexternal_scanner_state_id: %u", head->scanner_state_id);
    }

    fprintf(f, "\"]\n");
    array_push(&self->iterators, ((StackIterator) {
      .node = head->node
    }));
  }

  bool all_iterators_done = false;
  while (!all_iterators_done) {
    all_iterators_done = true;

    for (uint32_t i = 0; i < self->iterators.size; i++) {
      StackIterator iterator = *array_get(&self->iterators, i);
      StackNode *node = iterator.node;

      for (uint32_t j = 0; j < visited_nodes.size; j++) {
        if (*array_get(&visited_nodes, j) == node) {
          node = NULL;
          break;
        }
      }

      if (!node) continue;
      all_iterators_done = false;

      const StackLink *links = stack_node_links_const(node);
      fprintf(f, "node_%p [", (void *)node);
      if (node->state == ERROR_STATE) {
        fprintf(f, "label=\"?\"");
      } else if (
        node->link_count == 1 &&
        links[0].subtree.ptr &&
        ts_subtree_extra(links[0].subtree)
      ) {
        fprintf(f, "shape=point margin=0 label=\"\"");
      } else {
        fprintf(f, "label=\"%d\"", node->state);
      }

      fprintf(
        f,
        " tooltip=\"position: %u,%u\nnode_count:%u\nerror_cost: %u\ndynamic_precedence: %d\"];\n",
        node->position.extent.row + 1,
        node->position.extent.column,
        node->node_count,
        node->error_cost,
        node->dynamic_precedence
      );

      for (unsigned j = 0; j < node->link_count; j++) {
        StackLink link = links[j];
        fprintf(f, "node_%p -> node_%p [", (void *)node, (void *)link.node);
        if (link.subtree.ptr && ts_subtree_extra(link.subtree)) fprintf(f, "fontcolor=gray ");

        if (!link.subtree.ptr) {
          fprintf(f, "color=red");
        } else {
          fprintf(f, "label=\"");
          bool quoted = ts_subtree_visible(link.subtree) && !ts_subtree_named(link.subtree);
          if (quoted) fprintf(f, "'");
          ts_language_write_symbol_as_dot_string(language, f, ts_subtree_symbol(link.subtree));
          if (quoted) fprintf(f, "'");
          fprintf(f, "\"");
          fprintf(
            f,
            "labeltooltip=\"error_cost: %u\ndynamic_precedence: %" PRId32 "\"",
            ts_subtree_error_cost(link.subtree),
            ts_subtree_dynamic_precedence(link.subtree)
          );
        }

        fprintf(f, "];\n");

        StackIterator *next_iterator;
        if (j == 0) {
          next_iterator = array_get(&self->iterators, i);
        } else {
          array_push(&self->iterators, iterator);
          next_iterator = array_back(&self->iterators);
        }
        next_iterator->node = link.node;
      }

      array_push(&visited_nodes, node);
    }
  }

  fprintf(f, "}\n");

  array_delete(&visited_nodes);
  return true;
}

#undef forceinline

static void stack_node_expand_extra_groups(Stack *stack, StackNode *node) {
  StackLink *links = stack_node_links(node);
  for (unsigned i = 0; i < node->link_count; i++) {
    StackLink *link = &links[i];
    while (ts_subtree_stack_link_count(link->subtree) > 1) {
      ts_assert(ts_subtree_child_count(link->subtree) == 2);
      ts_assert(link->node->state == node->state);
      stack__invalidate_extra_chains(stack, node);
      Subtree group = link->subtree;
      const Subtree *children = ts_subtree_children(group);
      ts_subtree_retain(children[0]);
      ts_subtree_retain(children[1]);
      link->node = stack_node_new(link->node, children[0], node->state, &stack->node_pool);
      link->subtree = children[1];
      ts_subtree_release(stack->subtree_pool, group);
    }
  }
}

static void stack__skip_idle_steps(Stack *self) {
  uint32_t skip = UINT32_MAX;
  for (uint32_t i = 0; i < self->iterators.size; i++) {
    uint32_t steps = array_get(&self->iterators, i)->remaining_steps;
    if (steps < skip) skip = steps;
  }
  if (skip) {
    for (uint32_t i = 0; i < self->iterators.size; i++) {
      array_get(&self->iterators, i)->remaining_steps -= skip;
    }
  }
}
