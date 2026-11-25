#ifndef TREE_SITTER_PARSE_STACK_H_
#define TREE_SITTER_PARSE_STACK_H_

#ifdef __cplusplus
extern "C" {
#endif

#include "./array.h"
#include "./subtree.h"
#include <stdio.h>

typedef struct Stack Stack;

typedef unsigned StackVersion;
#define STACK_VERSION_NONE ((StackVersion)-1)

typedef struct {
  SubtreeArray subtrees;
  StackVersion version;
} StackSlice;
typedef Array(StackSlice) StackSliceArray;

typedef struct {
  Length position;
  unsigned depth;
  TSStateId state;
  bool has_root_subtree;
} StackSummaryEntry;
typedef Array(StackSummaryEntry) StackSummary;

// Create a stack.
Stack *ts_stack_new(SubtreePool *subtree_pool);

// Release the memory reserved for a given stack.
void ts_stack_delete(Stack *self);

// Get the stack's current number of versions.
uint32_t ts_stack_version_count(const Stack *self);

// Get the stack's current number of halted versions.
uint32_t ts_stack_halted_version_count(Stack *self);

// Get the state at the top of the given version of the stack. If the stack is
// empty, this returns the initial state, 1.
TSStateId ts_stack_state(const Stack *self, StackVersion version);

bool ts_stack_after_extra(const Stack *self, StackVersion version);

uint32_t ts_stack_external_scanner_state_id(const Stack *self, StackVersion version);

void ts_stack_set_external_scanner_state_id(Stack *self, StackVersion version, uint32_t state_id);

// Get the position of the given version of the stack within the document.
Length ts_stack_position(const Stack *, StackVersion);

void ts_stack_push_recovery(Stack *self, StackVersion version, Subtree subtree, TSStateId state, const TSLanguage *language);

// Push a tree and state onto the given version of the stack.
//
// This transfers ownership of the tree to the Stack. Callers that
// need to retain ownership of the tree for their own purposes should
// first retain the tree.
void ts_stack_push(Stack *self, StackVersion version, Subtree subtree, TSStateId state);

void ts_stack_record_ambiguous_extra(Stack *self);

// Pop the given number of entries from the given version of the stack. This
// operation can increase the number of stack versions by revealing multiple
// versions which had previously been merged. It returns an array that
// specifies the index of each revealed version and the trees that were
// removed from that version.
StackSliceArray ts_stack_pop_count(Stack *self, StackVersion version, uint32_t count);

typedef struct {
  bool has_multiple_paths;
  bool was_truncated;
} StackReductionStatus;

StackSliceArray ts_stack_pop_count_for_reduce(
  Stack *self,
  StackVersion version,
  uint32_t count,
  uint32_t max_reductions,
  StackReductionStatus *status
);

SubtreeHeapData *ts_stack_pop_count_in_place(
  Stack *self,
  StackVersion version,
  uint32_t count,
  uint32_t *child_count,
  SubtreeArray *trailing_extras
);

// Remove an error at the top of the given version of the stack.
SubtreeArray ts_stack_pop_error(Stack *self, StackVersion version);

/** Return false to pause the walk before its next traversal round. */
typedef bool (*StackProgressCallback)(void *payload, unsigned operations);

/**
 * Return false when paused; call again with the same version and unchanged stack
 * to resume. On true, result borrows Stack-owned slice storage. Clear each
 * subtrees entry after transferring its ownership; reset/delete releases entries
 * left unconsumed.
 */
bool ts_stack_pop_all(
  Stack *self,
  StackVersion version,
  StackSliceArray *result,
  StackProgressCallback progress,
  void *payload
);

// Get the maximum number of tree nodes reachable from this version of the stack
// since the last error was detected.
unsigned ts_stack_node_count_since_error(const Stack *self, StackVersion version);

// A nonzero generation identifying the error recovery this version resumed from, until the parser
// confirms the recovery; zero otherwise. Setting it assigns a fresh generation, copies inherit it,
// and merging with a confirmed version clears it.
uint64_t ts_stack_unconfirmed_recovery(const Stack *self, StackVersion version);

void ts_stack_set_unconfirmed_recovery(Stack *self, StackVersion version, bool value);

// Get the number of tree nodes reachable from this version beyond those reachable when its
// unconfirmed recovery was last set or cleared.
unsigned ts_stack_node_count_since_recovery(const Stack *self, StackVersion version);

// Whether the version holds a missing token that error recovery inserted although the lookahead
// is then shifted rather than reduced, and that neither a reduction nor an ERROR has incorporated
// yet.
bool ts_stack_has_unconfirmed_missing_token(const Stack *self, StackVersion version);

void ts_stack_set_unconfirmed_missing_token(Stack *self, StackVersion version, bool value);

int ts_stack_dynamic_precedence(Stack *self, StackVersion version);

bool ts_stack_has_advanced_since_error(const Stack *self, StackVersion version);

// The caller owns the returned array.
StackSummary ts_stack_current_summary(Stack *self, StackVersion version, unsigned max_depth);

void ts_stack_record_summary(Stack *self, StackVersion version, unsigned max_depth);

// Borrowed until the stack or its recorded summary changes.
StackSummary *ts_stack_get_summary(Stack *self, StackVersion version);

// Get the total cost of all errors on the given version of the stack.
unsigned ts_stack_error_cost(const Stack *self, StackVersion version);

// Merge the given two stack versions if possible, returning true
// if they were successfully merged and false otherwise.
bool ts_stack_merge(Stack *self, StackVersion version1, StackVersion version2);

// Determine whether the given two stack versions can be merged.
bool ts_stack_can_merge(Stack *self, StackVersion version1, StackVersion version2);

LookaheadToken ts_stack_resume(Stack *self, StackVersion version);

void ts_stack_pause(Stack *self, StackVersion version, LookaheadToken lookahead);

void ts_stack_halt(Stack *self, StackVersion version);

bool ts_stack_is_active(const Stack *self, StackVersion version);

bool ts_stack_is_paused(const Stack *self, StackVersion version);

bool ts_stack_is_halted(const Stack *self, StackVersion version);

void ts_stack_renumber_version(Stack *self, StackVersion v1, StackVersion v2);

void ts_stack_swap_versions(Stack *, StackVersion v1, StackVersion v2);

StackVersion ts_stack_copy_version(Stack *self, StackVersion version);

// Remove the given version from the stack.
void ts_stack_remove_version(Stack *self, StackVersion version);

void ts_stack_clear(Stack *self);

bool ts_stack_print_dot_graph(Stack *self, const TSLanguage *language, FILE *f);

#ifdef __cplusplus
}
#endif

#endif  // TREE_SITTER_PARSE_STACK_H_
