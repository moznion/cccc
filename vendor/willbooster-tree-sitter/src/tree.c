#include "tree_sitter/api.h"
#include "./atomic.h"
#include "./language.h"
#include "./length.h"
#include "./lexer.h"
#include "./subtree.h"
#include "./tree.h"

TSTree *ts_tree_new(
  Subtree root,
  const TSLanguage *language,
  const TSRange *included_ranges,
  unsigned included_range_count,
  TreeArenaSlab *arena
) {
  TSTree *result = ts_malloc(sizeof(TSTree));
  result->root = root;
  result->arena = arena;
  result->language = ts_language_copy(language);
#ifdef __wasm__
  result->language_context_id = ts_language_current_context_id();
  result->unparseable_language = NULL;
#endif
  if (
    included_range_count == 1 &&
    (included_ranges == &TS_DEFAULT_RANGE ||
     memcmp(included_ranges, &TS_DEFAULT_RANGE, sizeof(TSRange)) == 0)
  ) {
    result->included_ranges = NULL;
  } else {
    result->included_ranges = ts_calloc(included_range_count, sizeof(TSRange));
    memcpy(result->included_ranges, included_ranges, included_range_count * sizeof(TSRange));
  }
  result->included_range_count = included_range_count;
  return result;
}

TSTree *ts_tree_copy(const TSTree *self) {
  if (self->arena) {
    atomic_refcount_inc(&self->arena->ref_count);
  }
  TSTree *result = ts_tree_new(
    self->root,
    self->language,
    self->included_ranges ? self->included_ranges : &TS_DEFAULT_RANGE,
    self->included_range_count,
    self->arena
  );
#ifdef __wasm__
  result->language_context_id = self->language_context_id;
#endif
  return result;
}

void ts_tree_delete(TSTree *self) {
  if (!self) return;

  if (self->arena && atomic_refcount_dec_is_last(&self->arena->ref_count)) {
    ts_tree_arena_slab_free_all(self->arena);
  }
#ifdef __wasm__
  ts_language_delete(__atomic_load_n(&self->unparseable_language, __ATOMIC_ACQUIRE));
#endif
  ts_language_delete(self->language);
  ts_free(self->included_ranges);
  ts_free(self);
}

TSNode ts_tree_root_node(const TSTree *self) {
  return ts_node_new(self, &self->root, ts_subtree_padding(self->root), 0);
}

TSNode ts_tree_root_node_with_offset(
  const TSTree *self,
  uint32_t offset_bytes,
  TSPoint offset_extent
) {
  Length offset = {offset_bytes, offset_extent};
  return ts_node_new(self, &self->root, length_add(offset, ts_subtree_padding(self->root)), 0);
}

const TSLanguage *ts_tree_language(const TSTree *self) {
#ifdef __wasm__
  if (self->language_context_id != ts_language_current_context_id()) {
    TSTree *tree = (TSTree *)self;
    const TSLanguage *result = __atomic_load_n(
      &tree->unparseable_language,
      __ATOMIC_ACQUIRE
    );
    if (!result) {
      const TSLanguage *candidate = ts_language_copy_without_callbacks(self->language);
      if (__atomic_compare_exchange_n(
        &tree->unparseable_language,
        &result,
        candidate,
        false,
        __ATOMIC_RELEASE,
        __ATOMIC_ACQUIRE
      )) {
        result = candidate;
      } else {
        ts_language_delete(candidate);
      }
    }
    return result;
  }
#endif
  return self->language;
}

TSRange *ts_tree_included_ranges(const TSTree *self, uint32_t *length) {
  *length = self->included_range_count;
  TSRange *ranges = ts_calloc(self->included_range_count, sizeof(TSRange));
  if (self->included_ranges) {
    memcpy(ranges, self->included_ranges, self->included_range_count * sizeof(TSRange));
  } else {
    ranges[0] = TS_DEFAULT_RANGE;
  }
  return ranges;
}

#ifdef _WIN32

#include <io.h>
#include <windows.h>

int _ts_dup(HANDLE handle) {
  HANDLE dup_handle;
  if (!DuplicateHandle(
    GetCurrentProcess(), handle,
    GetCurrentProcess(), &dup_handle,
    0, FALSE, DUPLICATE_SAME_ACCESS
  )) return -1;

  return _open_osfhandle((intptr_t)dup_handle, 0);
}

void ts_tree_print_dot_graph(const TSTree *self, int fd) {
  FILE *file = _fdopen(_ts_dup((HANDLE)_get_osfhandle(fd)), "a");
  ts_subtree_print_dot_graph(self->root, self->language, file);
  fclose(file);
}

#elif !defined(__wasm__)

#include <unistd.h>

int _ts_dup(int file_descriptor) {
  return dup(file_descriptor);
}

void ts_tree_print_dot_graph(const TSTree *self, int file_descriptor) {
  FILE *file = fdopen(_ts_dup(file_descriptor), "a");
  ts_subtree_print_dot_graph(self->root, self->language, file);
  fclose(file);
}

#else

void ts_tree_print_dot_graph(const TSTree *self, int file_descriptor) {
  (void)self;
  (void)file_descriptor;
}

#endif
