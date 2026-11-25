// code-gauge-allow-file fileNcss -- TSNode accessors and child iterators share private inline helpers in one translation unit
#include <stdbool.h>
#include "./point.h"
#include "./subtree.h"
#include "./tree.h"
#include "./language.h"

typedef struct {
  Subtree parent;
  const TSTree *tree;
  Length position;
  uint32_t child_index;
  uint32_t structural_child_index;
  const TSSymbol *alias_sequence;
} NodeChildIterator;

typedef struct {
  TSNode parent;
  TSNode child;
} NodeAncestry;

static inline bool ts_node__is_relevant(TSNode self, bool include_anonymous);
static TSNode ts_node__first_child_without_fields(TSNode self);
static TSNode ts_node__child_with_descendant_unchecked(TSNode self, TSNode descendant);
static TSNode ts_node__child_with_descendant_checked(TSNode self, TSNode descendant);
static NodeAncestry ts_node__find_descendant(TSNode root, TSNode descendant, bool check_range);

// TSNode - constructors

TSNode ts_node_new(
  const TSTree *tree,
  const Subtree *subtree,
  Length position,
  TSSymbol alias
) {
  return (TSNode) {
    {position.bytes, position.extent.row, position.extent.column, alias},
    subtree,
    tree,
  };
}

static inline TSNode ts_node__null(void) {
  return ts_node_new(NULL, NULL, length_zero(), 0);
}

// TSNode - accessors

uint32_t ts_node_start_byte(TSNode self) {
  return self.context[0];
}

TSPoint ts_node_start_point(TSNode self) {
  return (TSPoint) {self.context[1], self.context[2]};
}

static inline uint32_t ts_node__alias(const TSNode *self) {
  return self->context[3];
}

static inline Subtree ts_node__subtree(TSNode self) {
  return *(const Subtree *)self.id;
}

// NodeChildIterator

static inline NodeChildIterator ts_node_iterate_children(const TSNode *node) {
  Subtree subtree = ts_node__subtree(*node);
  if (ts_subtree_child_count(subtree) == 0) {
    return (NodeChildIterator) {NULL_SUBTREE, node->tree, length_zero(), 0, 0, NULL};
  }
  const TSSymbol *alias_sequence = ts_language_alias_sequence(
    node->tree->language,
    ts_subtree_production_id(subtree)
  );
  return (NodeChildIterator) {
    .tree = node->tree,
    .parent = subtree,
    .position = {ts_node_start_byte(*node), ts_node_start_point(*node)},
    .child_index = 0,
    .structural_child_index = 0,
    .alias_sequence = alias_sequence,
  };
}

static inline bool ts_node_child_iterator_done(NodeChildIterator *self) {
  return self->child_index == self->parent.ptr->child_count;
}

static inline bool ts_node_child_iterator_next(
  NodeChildIterator *self,
  TSNode *result
) {
  if (!self->parent.ptr || ts_node_child_iterator_done(self)) return false;
  const Subtree *child = &ts_subtree_children(self->parent)[self->child_index];
  TSSymbol alias_symbol = 0;
  if (!ts_subtree_extra(*child)) {
    if (self->alias_sequence) {
      alias_symbol = self->alias_sequence[self->structural_child_index];
    }
    self->structural_child_index++;
  }
  if (self->child_index > 0) {
    self->position = length_add(self->position, ts_subtree_padding(*child));
  }
  *result = ts_node_new(
    self->tree,
    child,
    self->position,
    alias_symbol
  );
  self->position = length_add(self->position, ts_subtree_size(*child));
  self->child_index++;
  return true;
}

// TSNode - private

static inline bool ts_node__is_relevant(TSNode self, bool include_anonymous) {
  Subtree tree = ts_node__subtree(self);
  if (include_anonymous) {
    return ts_subtree_visible(tree) || ts_node__alias(&self);
  } else {
    TSSymbol alias = ts_node__alias(&self);
    if (alias) {
      return ts_language_symbol_metadata(self.tree->language, alias).named;
    } else {
      return ts_subtree_visible(tree) && ts_subtree_named(tree);
    }
  }
}

static inline uint32_t ts_node__relevant_child_count(
  TSNode self,
  bool include_anonymous
) {
  if (!include_anonymous && ts_node__is_relevant(self, true)) {
    return 0;
  }
  Subtree tree = ts_node__subtree(self);
  if (ts_subtree_child_count(tree) > 0) {
    if (include_anonymous) {
      return tree.ptr->visible_child_count;
    } else {
      return tree.ptr->named_child_count;
    }
  } else {
    return 0;
  }
}

static inline TSNode ts_node__child(
  TSNode self,
  uint32_t child_index,
  bool include_anonymous
) {
  TSNode result = self;
  bool did_descend = true;

  while (did_descend) {
    did_descend = false;

    TSNode child;
    uint32_t index = 0;
    NodeChildIterator iterator = ts_node_iterate_children(&result);
    while (ts_node_child_iterator_next(&iterator, &child)) {
      if (ts_node__is_relevant(child, include_anonymous)) {
        if (index == child_index) {
          return child;
        }
        index++;
      } else {
        uint32_t grandchild_index = child_index - index;
        uint32_t grandchild_count = ts_node__relevant_child_count(child, include_anonymous);
        if (grandchild_index < grandchild_count) {
          did_descend = true;
          result = child;
          child_index = grandchild_index;
          break;
        }
        index += grandchild_count;
      }
    }
  }

  return ts_node__null();
}

static bool ts_subtree_has_trailing_empty_descendant(
  Subtree self,
  const void *other_id
) {
  Array(Subtree) stack = array_new();
  Subtree subtree = self;

  while (true) {
    for (unsigned i = ts_subtree_child_count(subtree); i > 0; i--) {
      const Subtree *child_slot = &ts_subtree_children(subtree)[i - 1];
      Subtree child = *child_slot;
      if (ts_subtree_total_bytes(child) > 0) break;
      if ((const void *)child_slot == other_id) {
        array_delete(&stack);
        return true;
      }
      array_push(&stack, child);
    }

    if (!stack.size) break;
    subtree = array_pop(&stack);
  }

  array_delete(&stack);
  return false;
}

// code-gauge-allow functionCognitiveComplexity -- cursor-free sibling ascent tracks candidate nodes across hidden wrappers
// code-gauge-allow functionNcss -- cursor-free sibling ascent tracks candidate nodes across hidden wrappers
static inline TSNode ts_node__prev_sibling(TSNode self, bool include_anonymous) {
  Subtree self_subtree = ts_node__subtree(self);
  bool self_is_empty = ts_subtree_total_bytes(self_subtree) == 0;
  uint32_t target_end_byte = ts_node_end_byte(self);

  TSNode node = ts_node_parent(self);
  TSNode earlier_node = ts_node__null();
  bool earlier_node_is_relevant = false;

  while (!ts_node_is_null(node)) {
    TSNode earlier_child = ts_node__null();
    bool earlier_child_is_relevant = false;
    bool found_child_containing_target = false;

    TSNode child;
    NodeChildIterator iterator = ts_node_iterate_children(&node);
    while (ts_node_child_iterator_next(&iterator, &child)) {
      if (child.id == self.id) break;
      if (iterator.position.bytes > target_end_byte) {
        found_child_containing_target = true;
        break;
      }

      if (iterator.position.bytes == target_end_byte &&
          (!self_is_empty ||
           ts_subtree_has_trailing_empty_descendant(ts_node__subtree(child), self.id))) {
        found_child_containing_target = true;
        break;
      }

      if (ts_node__is_relevant(child, include_anonymous)) {
        earlier_child = child;
        earlier_child_is_relevant = true;
      } else if (ts_node__relevant_child_count(child, include_anonymous) > 0) {
        earlier_child = child;
        earlier_child_is_relevant = false;
      }
    }

    if (found_child_containing_target) {
      if (!ts_node_is_null(earlier_child)) {
        earlier_node = earlier_child;
        earlier_node_is_relevant = earlier_child_is_relevant;
      }
      node = child;
    } else if (earlier_child_is_relevant) {
      return earlier_child;
    } else if (!ts_node_is_null(earlier_child)) {
      node = earlier_child;
    } else if (earlier_node_is_relevant) {
      return earlier_node;
    } else {
      node = earlier_node;
      earlier_node = ts_node__null();
      earlier_node_is_relevant = false;
    }
  }

  return ts_node__null();
}

// code-gauge-allow functionCognitiveComplexity -- cursor-free sibling ascent tracks candidate nodes across hidden wrappers
// code-gauge-allow functionNcss -- cursor-free sibling ascent tracks candidate nodes across hidden wrappers
static inline TSNode ts_node__next_sibling(TSNode self, bool include_anonymous) {
  uint32_t target_end_byte = ts_node_end_byte(self);

  TSNode node = ts_node_parent(self);
  TSNode later_node = ts_node__null();
  bool later_node_is_relevant = false;

  while (!ts_node_is_null(node)) {
    TSNode later_child = ts_node__null();
    bool later_child_is_relevant = false;
    TSNode child_containing_target = ts_node__null();

    TSNode child;
    NodeChildIterator iterator = ts_node_iterate_children(&node);
    while (ts_node_child_iterator_next(&iterator, &child)) {
      if (iterator.position.bytes <= target_end_byte) continue;
      uint32_t start_byte = ts_node_start_byte(self);
      uint32_t child_start_byte = ts_node_start_byte(child);

      bool is_empty = start_byte == target_end_byte;
      bool contains_target = is_empty ?
        child_start_byte < start_byte :
        child_start_byte <= start_byte;

      if (contains_target) {
        if (child.id != self.id) {
          child_containing_target = child;
        }
      } else if (ts_node__is_relevant(child, include_anonymous)) {
        later_child = child;
        later_child_is_relevant = true;
        break;
      } else if (ts_node__relevant_child_count(child, include_anonymous) > 0) {
        later_child = child;
        later_child_is_relevant = false;
        break;
      }
    }

    if (!ts_node_is_null(child_containing_target)) {
      if (!ts_node_is_null(later_child)) {
        later_node = later_child;
        later_node_is_relevant = later_child_is_relevant;
      }
      node = child_containing_target;
    } else if (later_child_is_relevant) {
      return later_child;
    } else if (!ts_node_is_null(later_child)) {
      node = later_child;
    } else if (later_node_is_relevant) {
      return later_node;
    } else {
      node = later_node;
    }
  }

  return ts_node__null();
}

static inline TSNode ts_node__first_child_for_byte(
  TSNode self,
  uint32_t goal,
  bool include_anonymous
) {
  TSNode node = self;
  bool did_descend = true;

  Array(NodeChildIterator) iterators = array_new();

  while (did_descend) {
    did_descend = false;

    TSNode child;
    NodeChildIterator iterator = ts_node_iterate_children(&node);
  loop:
    while (ts_node_child_iterator_next(&iterator, &child)) {
      if (ts_node_end_byte(child) > goal) {
        if (ts_node__is_relevant(child, include_anonymous)) {
          array_delete(&iterators);
          return child;
        } else if (ts_node__relevant_child_count(child, include_anonymous) > 0) {
          if (!ts_node_child_iterator_done(&iterator)) {
            array_push(&iterators, iterator);
          }
          did_descend = true;
          node = child;
          break;
        }
      }
    }

    if (!did_descend && iterators.size > 0) {
      iterator = array_pop(&iterators);
      goto loop;
    }
  }

  array_delete(&iterators);
  return ts_node__null();
}

static inline bool ts_node__length_matches(Length a, Length b, bool use_points) {
  return use_points ? point_eq(a.extent, b.extent) : a.bytes == b.bytes;
}

static inline bool ts_node__starts_at(TSNode self, Length position, bool use_points) {
  Length start = {ts_node_start_byte(self), ts_node_start_point(self)};
  return ts_node__length_matches(start, position, use_points);
}

// Hidden children ending at this node's end may contain empty visible children.
// Track the public child index so exhausted hidden branches retain later siblings.
static TSNode ts_node__first_empty_child_at_end(
  TSNode self,
  bool use_points
) {
  TSNode node = self;
  uint32_t child_index = 0;
  Length end = {ts_node_end_byte(self), ts_node_end_point(self)};
  bool did_descend = true;

  while (did_descend) {
    did_descend = false;
    TSNode child;
    NodeChildIterator iterator = ts_node_iterate_children(&node);
    while (ts_node_child_iterator_next(&iterator, &child)) {
      bool relevant = ts_node__is_relevant(child, true);
      uint32_t count = relevant ? 1 : ts_node__relevant_child_count(child, true);
      if (ts_node__length_matches(iterator.position, end, use_points)) {
        if (relevant && ts_node__starts_at(child, end, use_points)) return child;
        if (!relevant && count > 0) {
          node = child;
          did_descend = true;
          break;
        }
      }
      child_index += count;
    }
  }

  TSNode child = ts_node__child(self, child_index, true);
  if (ts_node_is_null(child) || !ts_node__starts_at(child, end, use_points)) {
    return ts_node__null();
  }
  return child;
}

static inline TSNode ts_node__descendant_for_byte_range(
  TSNode self,
  uint32_t range_start,
  uint32_t range_end,
  bool include_anonymous
) {
  if (range_start > range_end) {
    return ts_node__null();
  }
  TSNode node = self;
  TSNode last_visible_node = self;

  bool did_descend = true;
  while (did_descend) {
    did_descend = false;

    TSNode child;
    NodeChildIterator iterator = ts_node_iterate_children(&node);
    while (ts_node_child_iterator_next(&iterator, &child)) {
      if (!ts_node__is_relevant(child, true) && ts_node_child_count(child) == 0) continue;
      uint32_t node_end = iterator.position.bytes;

      if (node_end < range_end) continue;

      if (node_end == range_start && ts_node_start_byte(child) != node_end) {
        if (ts_node__is_relevant(child, true)) continue;
        child = ts_node__first_empty_child_at_end(child, false);
        if (ts_node_is_null(child)) continue;
      }

      if (range_start < ts_node_start_byte(child)) break;

      node = child;
      if (ts_node__is_relevant(node, include_anonymous)) {
        last_visible_node = node;
      }
      did_descend = true;
      break;
    }
  }

  return last_visible_node;
}

static inline TSNode ts_node__descendant_for_point_range(
  TSNode self,
  TSPoint range_start,
  TSPoint range_end,
  bool include_anonymous
) {
  if (point_gt(range_start, range_end)) {
    return ts_node__null();
  }
  TSNode node = self;
  TSNode last_visible_node = self;

  bool did_descend = true;
  while (did_descend) {
    did_descend = false;

    TSNode child;
    NodeChildIterator iterator = ts_node_iterate_children(&node);
    while (ts_node_child_iterator_next(&iterator, &child)) {
      if (!ts_node__is_relevant(child, true) && ts_node_child_count(child) == 0) continue;
      TSPoint node_end = iterator.position.extent;

      if (point_lt(node_end, range_end)) continue;

      if (point_eq(node_end, range_start) && !point_eq(ts_node_start_point(child), node_end)) {
        if (ts_node__is_relevant(child, true)) continue;
        child = ts_node__first_empty_child_at_end(child, true);
        if (ts_node_is_null(child)) continue;
      }

      if (point_lt(range_start, ts_node_start_point(child))) break;

      node = child;
      if (ts_node__is_relevant(node, include_anonymous)) {
        last_visible_node = node;
      }
      did_descend = true;
      break;
    }
  }

  return last_visible_node;
}

// TSNode - public

uint32_t ts_node_end_byte(TSNode self) {
  return ts_node_start_byte(self) + ts_subtree_size(ts_node__subtree(self)).bytes;
}

TSPoint ts_node_end_point(TSNode self) {
  return point_add(ts_node_start_point(self), ts_subtree_size(ts_node__subtree(self)).extent);
}

TSSymbol ts_node_symbol(TSNode self) {
  TSSymbol symbol = ts_node__alias(&self);
  if (!symbol) symbol = ts_subtree_symbol(ts_node__subtree(self));
  return ts_language_public_symbol(self.tree->language, symbol);
}

const char *ts_node_type(TSNode self) {
  TSSymbol symbol = ts_node__alias(&self);
  if (!symbol) symbol = ts_subtree_symbol(ts_node__subtree(self));
  return ts_language_symbol_name(self.tree->language, symbol);
}

const TSLanguage *ts_node_language(TSNode self) {
  return ts_tree_language(self.tree);
}

TSSymbol ts_node_grammar_symbol(TSNode self) {
  return ts_subtree_symbol(ts_node__subtree(self));
}

const char *ts_node_grammar_type(TSNode self) {
  TSSymbol symbol = ts_subtree_symbol(ts_node__subtree(self));
  return ts_language_symbol_name(self.tree->language, symbol);
}

char *ts_node_string(TSNode self) {
  TSSymbol alias_symbol = ts_node__alias(&self);
  return ts_subtree_string(
    ts_node__subtree(self),
    alias_symbol,
    ts_language_symbol_metadata(self.tree->language, alias_symbol).visible,
    self.tree->language,
    (SubtreeStringOptions) {0}
  );
}

char *ts_node_string_pretty(TSNode self, size_t initial_indent_level) {
  TSSymbol alias_symbol = ts_node__alias(&self);
  return ts_subtree_string(
    ts_node__subtree(self),
    alias_symbol,
    ts_language_symbol_metadata(self.tree->language, alias_symbol).visible,
    self.tree->language,
    (SubtreeStringOptions) {.pretty = true, .initial_indent_level = initial_indent_level}
  );
}

bool ts_node_eq(TSNode self, TSNode other) {
  return self.tree == other.tree && self.id == other.id;
}

bool ts_node_is_null(TSNode self) {
  return self.id == 0;
}

bool ts_node_is_extra(TSNode self) {
  return ts_subtree_extra(ts_node__subtree(self));
}

bool ts_node_is_named(TSNode self) {
  TSSymbol alias = ts_node__alias(&self);
  return alias
    ? ts_language_symbol_metadata(self.tree->language, alias).named
    : ts_subtree_named(ts_node__subtree(self));
}

bool ts_node_is_missing(TSNode self) {
  return ts_subtree_missing(ts_node__subtree(self));
}

bool ts_node_has_error(TSNode self) {
  Subtree subtree = ts_node__subtree(self);
  return ts_subtree_is_error(subtree) || ts_subtree_error_cost(subtree) > 0;
}

bool ts_node_is_error(TSNode self) {
  TSSymbol symbol = ts_node_symbol(self);
  return symbol == ts_builtin_sym_error;
}

uint32_t ts_node_descendant_count(TSNode self) {
  return ts_subtree_visible_descendant_count(ts_node__subtree(self)) + 1;
}

TSNode ts_node_parent(TSNode self) {
  TSNode node = ts_tree_root_node(self.tree);
  if (node.id == self.id) return ts_node__null();
  if (ts_node_start_byte(self) == ts_node_end_byte(self)) {
    TSNode parent = ts_node__find_descendant(node, self, true).parent;
    if (ts_node_is_null(parent)) {
      // Offset-root nodes can lie outside their ancestors' unshifted ranges.
      parent = ts_node__find_descendant(node, self, false).parent;
    }
    return parent;
  }

  while (true) {
    TSNode next_node = ts_node__child_with_descendant_unchecked(node, self);
    if (next_node.id == self.id || ts_node_is_null(next_node)) break;
    node = next_node;
  }

  return node;
}

TSNode ts_node_child_with_descendant(TSNode self, TSNode descendant) {
  if (self.tree != descendant.tree || self.id == descendant.id) {
    return ts_node__null();
  }
  uint32_t start_byte = ts_node_start_byte(descendant);
  uint32_t end_byte = ts_node_end_byte(descendant);
  if (
    start_byte == end_byte
    || (start_byte == ts_node_start_byte(self) && end_byte == ts_node_end_byte(self))
  ) {
    return ts_node__child_with_descendant_checked(self, descendant);
  }
  return ts_node__child_with_descendant_unchecked(self, descendant);
}

static TSNode ts_node__child_with_descendant_unchecked(TSNode self, TSNode descendant) {
  uint32_t start_byte = ts_node_start_byte(descendant);
  uint32_t end_byte = ts_node_end_byte(descendant);
  if (start_byte == end_byte) {
    return ts_node__child_with_descendant_checked(self, descendant);
  }

  do {
    NodeChildIterator iter = ts_node_iterate_children(&self);
    do {
      if (
        !ts_node_child_iterator_next(&iter, &self)
        || ts_node_start_byte(self) > start_byte
      ) {
        return ts_node__null();
      }
      if (self.id == descendant.id) {
        return self;
      }

    } while (iter.position.bytes < end_byte || ts_node_child_count(self) == 0);
  } while (!ts_node__is_relevant(self, true));

  return self;
}

static TSNode ts_node__child_with_descendant_checked(TSNode self, TSNode descendant) {
  return ts_node__find_descendant(self, descendant, true).child;
}

static NodeAncestry ts_node__find_descendant(TSNode root, TSNode descendant, bool check_range) {
  typedef struct {
    NodeChildIterator iterator;
    TSNode parent;
    TSNode child;
  } Entry;
  Array(Entry) stack = array_new();
  array_push(&stack, ((Entry) {ts_node_iterate_children(&root), root, ts_node__null()}));
  uint32_t start_byte = ts_node_start_byte(descendant);
  uint32_t end_byte = ts_node_end_byte(descendant);
  NodeAncestry result = {ts_node__null(), ts_node__null()};

  while (stack.size) {
    Entry *entry = array_back(&stack);
    TSNode node;
    if (!ts_node_child_iterator_next(&entry->iterator, &node)) {
      stack.size--;
      continue;
    }
    if (check_range && (ts_node_start_byte(node) > start_byte || ts_node_end_byte(node) < end_byte)) {
      continue;
    }
    bool visible = ts_node__is_relevant(node, true);
    TSNode first_child = entry->child;
    if (visible && ts_node_is_null(first_child)) first_child = node;
    if (node.id == descendant.id) {
      result = (NodeAncestry) {entry->parent, first_child};
      break;
    }
    if (ts_subtree_child_count(ts_node__subtree(node))) {
      Entry next = {
        ts_node_iterate_children(&node),
        visible ? node : entry->parent,
        first_child,
      };
      array_push(&stack, next);
    }
  }

  array_delete(&stack);
  return result;
}

TSNode ts_node_child(TSNode self, uint32_t child_index) {
  return ts_node__child(self, child_index, true);
}

TSNode ts_node_named_child(TSNode self, uint32_t child_index) {
  return ts_node__child(self, child_index, false);
}

TSNode ts_node_child_by_field_id(TSNode self, TSFieldId field_id) {
  typedef struct {
    TSNode parent;
    NodeChildIterator iterator;
    const TSFieldMapEntry *field_map;
    const TSFieldMapEntry *field_map_end;
  } FieldSearchFrame;

  if (!field_id) return ts_node__null();

  Array(FieldSearchFrame) stack = array_new();
  NodeChildIterator iterator;
  const TSFieldMapEntry *field_map;
  const TSFieldMapEntry *field_map_end;
  TSNode child;

descend:
  if (ts_node_child_count(self) == 0) goto not_found;

  ts_language_field_map(
    self.tree->language,
    ts_subtree_production_id(ts_node__subtree(self)),
    &field_map,
    &field_map_end
  );
  if (field_map == field_map_end) goto not_found;

  // The field mappings are sorted by their field id. Scan all
  // the mappings to find the ones for the given field id.
  while (field_map->field_id < field_id) {
    field_map++;
    if (field_map == field_map_end) goto not_found;
  }
  while (field_map_end[-1].field_id > field_id) {
    field_map_end--;
    if (field_map == field_map_end) goto not_found;
  }

  iterator = ts_node_iterate_children(&self);

scan:
  while (ts_node_child_iterator_next(&iterator, &child)) {
    if (!ts_subtree_extra(ts_node__subtree(child))) {
      uint32_t index = iterator.structural_child_index - 1;
      if (index < field_map->child_index) continue;

      if (field_map->inherited) {
        if (ts_node__is_relevant(child, true)) {
          field_map++;
          if (field_map == field_map_end) goto not_found;
          continue;
        }

        if (field_map + 1 == field_map_end) {
          self = child;
          goto descend;
        }

        else {
          array_push(&stack, ((FieldSearchFrame) {
            .parent = self,
            .iterator = iterator,
            .field_map = field_map,
            .field_map_end = field_map_end,
          }));
          self = child;
          goto descend;
        }
      }

      else if (ts_node__is_relevant(child, true)) {
        array_delete(&stack);
        return child;
      }

      else if (ts_node_child_count(child) > 0) {
        TSNode first_child = ts_node__first_child_without_fields(child);
        if (first_child.id) {
          array_delete(&stack);
          return first_child;
        }
        TSTreeCursor cursor = ts_tree_cursor_new(self);
        TSNode result = ts_node__null();
        if (ts_tree_cursor_goto_first_child(&cursor)) {
          do {
            if (ts_tree_cursor_current_field_id(&cursor) == field_id) {
              result = ts_tree_cursor_current_node(&cursor);
              break;
            }
          } while (ts_tree_cursor_goto_next_sibling(&cursor));
        }
        ts_tree_cursor_delete(&cursor);
        if (ts_node_is_null(result)) goto not_found;
        array_delete(&stack);
        return result;
      }

      else {
        field_map++;
        if (field_map == field_map_end) goto not_found;
      }
    }
  }

not_found:
  if (stack.size) {
    FieldSearchFrame frame = array_pop(&stack);
    self = frame.parent;
    iterator = frame.iterator;
    field_map = frame.field_map + 1;
    field_map_end = frame.field_map_end;
    if (field_map == field_map_end) goto not_found;
    goto scan;
  }

  array_delete(&stack);
  return ts_node__null();
}

// code-gauge-allow functionCognitiveComplexity -- walks down single-child fieldless wrappers in one loop
static TSNode ts_node__first_child_without_fields(TSNode self) {
  for (;;) {
    const TSFieldMapEntry *field_map, *field_map_end;
    ts_language_field_map(
      self.tree->language,
      ts_subtree_production_id(ts_node__subtree(self)),
      &field_map,
      &field_map_end
    );
    if (field_map != field_map_end) return ts_node__null();

    TSNode child;
    NodeChildIterator iterator = ts_node_iterate_children(&self);
    bool descended = false;
    while (ts_node_child_iterator_next(&iterator, &child)) {
      bool visible = ts_node__is_relevant(child, true);
      if (!visible && ts_node_child_count(child) == 0) continue;
      if (ts_subtree_extra(ts_node__subtree(child))) return ts_node__null();
      if (visible) return child;
      self = child;
      descended = true;
      break;
    }
    if (!descended) return ts_node__null();
  }
}

static inline const char *ts_node__field_name_from_language(TSNode self, uint32_t structural_child_index) {
    const TSFieldMapEntry *field_map, *field_map_end;
    ts_language_field_map(
      self.tree->language,
      ts_subtree_production_id(ts_node__subtree(self)),
      &field_map,
      &field_map_end
    );
    for (; field_map != field_map_end; field_map++) {
      if (!field_map->inherited && field_map->child_index == structural_child_index) {
        return ts_language_field_name_for_id(self.tree->language, field_map->field_id);
      }
    }
    return NULL;
}

// Returns the field that the nodes inside the hidden `child` of `parent` inherit.
static const char *ts_node__field_name_inherited_by(
  TSNode parent,
  TSNode child,
  uint32_t next_structural_child_index,
  const char *inherited_field_name
) {
  // An extra takes no structural index, and the cursor gives the nodes inside
  // one no field from outside it.
  if (ts_node_is_extra(child)) return NULL;
  const char *field_name = ts_node__field_name_from_language(parent, next_structural_child_index - 1);
  return field_name ? field_name : inherited_field_name;
}

const char *ts_node_field_name_for_child(TSNode self, uint32_t child_index) {
  TSNode result = self;
  bool did_descend = true;
  const char *inherited_field_name = NULL;

  while (did_descend) {
    did_descend = false;

    TSNode child;
    uint32_t index = 0;
    NodeChildIterator iterator = ts_node_iterate_children(&result);
    while (ts_node_child_iterator_next(&iterator, &child)) {
      if (ts_node__is_relevant(child, true)) {
        if (index == child_index) {
          if (ts_node_is_extra(child)) {
            return NULL;
          }
          const char *field_name = ts_node__field_name_from_language(result, iterator.structural_child_index - 1);
          if (field_name) return field_name;
          return inherited_field_name;
        }
        index++;
      } else {
        uint32_t grandchild_index = child_index - index;
        uint32_t grandchild_count = ts_node__relevant_child_count(child, true);
        if (grandchild_index < grandchild_count) {
          inherited_field_name = ts_node__field_name_inherited_by(
            result, child, iterator.structural_child_index, inherited_field_name
          );

          did_descend = true;
          result = child;
          child_index = grandchild_index;
          break;
        }
        index += grandchild_count;
      }
    }
  }

  return NULL;
}

const char *ts_node_field_name_for_named_child(TSNode self, uint32_t named_child_index) {
  TSNode result = self;
  bool did_descend = true;
  const char *inherited_field_name = NULL;

  while (did_descend) {
    did_descend = false;

    TSNode child;
    uint32_t index = 0;
    NodeChildIterator iterator = ts_node_iterate_children(&result);
    while (ts_node_child_iterator_next(&iterator, &child)) {
      if (ts_node__is_relevant(child, false)) {
        if (index == named_child_index) {
          if (ts_node_is_extra(child)) {
            return NULL;
          }
          const char *field_name = ts_node__field_name_from_language(result, iterator.structural_child_index - 1);
          if (field_name) return field_name;
          return inherited_field_name;
        }
        index++;
      } else {
        uint32_t named_grandchild_index = named_child_index - index;
        uint32_t grandchild_count = ts_node__relevant_child_count(child, false);
        if (named_grandchild_index < grandchild_count) {
          inherited_field_name = ts_node__field_name_inherited_by(
            result, child, iterator.structural_child_index, inherited_field_name
          );

          did_descend = true;
          result = child;
          named_child_index = named_grandchild_index;
          break;
        }
        index += grandchild_count;
      }
    }
  }

  return NULL;
}

TSNode ts_node_child_by_field_name(
  TSNode self,
  const char *name,
  uint32_t name_length
) {
  TSFieldId field_id = ts_language_field_id_for_name(
    self.tree->language,
    name,
    name_length
  );
  return ts_node_child_by_field_id(self, field_id);
}

uint32_t ts_node_child_count(TSNode self) {
  Subtree tree = ts_node__subtree(self);
  if (ts_subtree_child_count(tree) > 0) {
    return tree.ptr->visible_child_count;
  } else {
    return 0;
  }
}

uint32_t ts_node_named_child_count(TSNode self) {
  Subtree tree = ts_node__subtree(self);
  if (ts_subtree_child_count(tree) > 0) {
    return tree.ptr->named_child_count;
  } else {
    return 0;
  }
}

TSNode ts_node_next_sibling(TSNode self) {
  return ts_node__next_sibling(self, true);
}

TSNode ts_node_next_named_sibling(TSNode self) {
  return ts_node__next_sibling(self, false);
}

TSNode ts_node_prev_sibling(TSNode self) {
  return ts_node__prev_sibling(self, true);
}

TSNode ts_node_prev_named_sibling(TSNode self) {
  return ts_node__prev_sibling(self, false);
}

TSNode ts_node_first_child_for_byte(TSNode self, uint32_t byte) {
  return ts_node__first_child_for_byte(self, byte, true);
}

TSNode ts_node_first_named_child_for_byte(TSNode self, uint32_t byte) {
  return ts_node__first_child_for_byte(self, byte, false);
}

TSNode ts_node_descendant_for_byte_range(
  TSNode self,
  uint32_t start,
  uint32_t end
) {
  return ts_node__descendant_for_byte_range(self, start, end, true);
}

TSNode ts_node_named_descendant_for_byte_range(
  TSNode self,
  uint32_t start,
  uint32_t end
) {
  return ts_node__descendant_for_byte_range(self, start, end, false);
}

TSNode ts_node_descendant_for_point_range(
  TSNode self,
  TSPoint start,
  TSPoint end
) {
  return ts_node__descendant_for_point_range(self, start, end, true);
}

TSNode ts_node_named_descendant_for_point_range(
  TSNode self,
  TSPoint start,
  TSPoint end
) {
  return ts_node__descendant_for_point_range(self, start, end, false);
}
