#include "./lex_table.h"

#include <string.h>

#define TS_LEX_DEAD_END_INIT_SLOTS 1024u
#define TS_LEX_DEAD_END_INIT_ENDS 16u

typedef struct {
  uint32_t byte_plus_1;
  uint16_t state;
  uint16_t end_idx;
} TSLexDeadEndSlot;

typedef struct {
  uint32_t byte;
  uint16_t state;
} LexDeadSample;

typedef struct {
  LexDeadSample *items;
  uint32_t len;
  uint32_t cap;
  uint32_t last_block;
} LexDeadSampleList;

struct TSLexDeadEndCache {
  const uint16_t *table;
  TSLexDeadEndSlot *slots;
  TSLexDeadEndTarget *ends;
  uint32_t slot_mask;
  uint32_t slot_count;
  uint16_t end_count;
  uint16_t end_cap;
};

static void ts_lex_free_dead_cache(Lexer *self);
static TSLexDeadEndTarget ts_lex_collect_dead_samples(
  Lexer *self, const TSLexTableView *v, uint16_t state, LexDeadSampleList *list
);
static bool ts_lex_sample_dead_block(
  const Lexer *self, const TSLexTableView *v, uint16_t state, TSLexStateMeta sm,
  LexDeadSampleList *list, TSLexDeadEndTarget *cached
);
static inline uint16_t ts_lex_replay_action(
  const TSLexTableView *v, TSLexStateMeta sm, int32_t lookahead, bool eof
);
static void ts_lex_push_dead_sample(LexDeadSampleList *list, uint32_t byte, uint16_t state);
static TSLexDeadEndCache *ts_lex_ensure_dead_cache(Lexer *self, const uint16_t *table);
static uint16_t ts_lex_intern_dead_end(TSLexDeadEndCache *cache, TSLexDeadEndTarget target);
static void ts_lex_insert_dead_slot(
  TSLexDeadEndCache *cache, uint16_t state, uint32_t byte, uint16_t end_idx
);
static void ts_lex_grow_dead_slots(TSLexDeadEndCache *cache);
static inline uint32_t ts_lex_dead_hash(uint16_t state, uint32_t byte);

void ts_lexer_clear_cache(Lexer *self) {
  if (self->main_lex_cache.unicode_exc) ts_free(self->main_lex_cache.unicode_exc);
  if (self->kw_lex_cache.unicode_exc) ts_free(self->kw_lex_cache.unicode_exc);
  self->main_lex_cache = (TSLexTableView){0};
  self->kw_lex_cache = (TSLexTableView){0};
  ts_lexer_clear_dead_ends(self);
}

void ts_lexer_clear_dead_ends(Lexer *self) {
  ts_lex_free_dead_cache(self);
  self->ext_eof_guard = (TSLexExtEofGuard){0};
  memset(self->ext_scan_cache, 0, sizeof(self->ext_scan_cache));
}

static void ts_lex_free_dead_cache(Lexer *self) {
  if (!self->dead_ends) return;
  ts_free(self->dead_ends->slots);
  ts_free(self->dead_ends->ends);
  ts_free(self->dead_ends);
  self->dead_ends = NULL;
}

bool ts_lex_lookup_dead_end(
  const TSLexDeadEndCache *cache,
  const uint16_t *table,
  uint16_t state,
  uint32_t byte,
  TSLexDeadEndTarget *target
) {
  if (!cache || cache->table != table || byte == UINT32_MAX) return false;
  uint32_t key = byte + 1u;
  uint32_t mask = cache->slot_mask;
  uint32_t idx = ts_lex_dead_hash(state, key) & mask;
  for (;;) {
    TSLexDeadEndSlot slot = cache->slots[idx];
    if (slot.byte_plus_1 == 0) return false;
    if (slot.byte_plus_1 == key && slot.state == state) {
      *target = cache->ends[slot.end_idx];
      return true;
    }
    idx = (idx + 1u) & mask;
  }
}

TS_NOINLINE void ts_lex_record_dead_ends(
  Lexer *self,
  const TSLexTableView *v,
  uint16_t start_state,
  Length start_pos
) {
  Length final_pos = self->current_position;
  ColumnData saved_col = self->column_data;
  ColumnData saved_disp = self->display_column_data;

  ts_lexer_goto_dead_end(self, start_pos);
  if (self->track_columns && self->start_column_checkpoint.position.bytes == start_pos.bytes) {
    self->column_data = self->start_column_checkpoint.column;
    self->display_column_data = self->start_column_checkpoint.display_column;
  }
  LexDeadSampleList list = {NULL, 0, 0, UINT32_MAX};
  TSLexDeadEndTarget target = ts_lex_collect_dead_samples(self, v, start_state, &list);

  if (!self->aborted && list.len > 0) {
    TSLexDeadEndCache *cache = ts_lex_ensure_dead_cache(self, v->table);
    uint16_t end_idx = ts_lex_intern_dead_end(cache, target);
    if (end_idx != UINT16_MAX) {
      for (uint32_t i = 0; i < list.len; i++) {
        ts_lex_insert_dead_slot(cache, list.items[i].state, list.items[i].byte, end_idx);
      }
    }
  }
  ts_free(list.items);
  if (!self->aborted) {
    ts_lexer_goto_dead_end(self, final_pos);
    self->column_data = saved_col;
    self->display_column_data = saved_disp;
  }
}

static TSLexDeadEndTarget ts_lex_collect_dead_samples(
  Lexer *self,
  const TSLexTableView *v,
  uint16_t state,
  LexDeadSampleList *list
) {
  uint32_t eof_steps = 0;
  for (;;) {
    TSLexStateMeta sm = ts_lex_state_meta(v, state);
    TSLexDeadEndTarget cached;
    if (ts_lex_sample_dead_block(self, v, state, sm, list, &cached)) return cached;
    bool eof = self->data.lookahead == 0 && self->data.eof(&self->data);
    uint16_t act = ts_lex_replay_action(v, sm, self->data.lookahead, eof);
    if (act == 0 || (eof && ++eof_steps > v->state_count)) break;
    if (ts_lex_action_is_skip(v, act)) {
      list->len = 0;
      list->last_block = UINT32_MAX;
    }
    state = ts_lex_action_next_state(v, act);
    self->data.advance(&self->data, false);
    if (self->aborted) break;
  }
  return (TSLexDeadEndTarget){self->current_position, self->column_data, self->display_column_data};
}

static bool ts_lex_sample_dead_block(
  const Lexer *self,
  const TSLexTableView *v,
  uint16_t state,
  TSLexStateMeta sm,
  LexDeadSampleList *list,
  TSLexDeadEndTarget *cached
) {
  if (sm.acc != 0) {
    list->len = 0;
    list->last_block = UINT32_MAX;
    return false;
  }
  uint32_t cur_byte = self->current_position.bytes;
  uint32_t block = cur_byte >> 6;
  if (block == list->last_block) return false;
  if (ts_lex_lookup_dead_end(self->dead_ends, v->table, state, cur_byte, cached)) return true;
  list->last_block = block;
  ts_lex_push_dead_sample(list, cur_byte, state);
  return false;
}

static inline uint16_t ts_lex_replay_action(
  const TSLexTableView *v,
  TSLexStateMeta sm,
  int32_t lookahead,
  bool eof
) {
  if (!eof && (uint32_t)lookahead >= 0x110000u) {
    uint16_t act = ts_lex_lookup_action(v, sm, v->replacement_class);
    return ts_lex_action_is_skip(v, act) ? 0 : act;
  }
  return ts_lex_lookup_action(v, sm, ts_lex_char_class(v, lookahead, eof));
}

static void ts_lex_push_dead_sample(LexDeadSampleList *list, uint32_t byte, uint16_t state) {
  if (list->len == list->cap) {
    uint32_t next_cap = list->cap ? list->cap * 2u : 64u;
    list->items = ts_realloc(list->items, (size_t)next_cap * sizeof(LexDeadSample));
    list->cap = next_cap;
  }
  list->items[list->len++] = (LexDeadSample){byte, state};
}

static TSLexDeadEndCache *ts_lex_ensure_dead_cache(Lexer *self, const uint16_t *table) {
  if (self->dead_ends && self->dead_ends->table != table) ts_lex_free_dead_cache(self);
  if (!self->dead_ends) {
    TSLexDeadEndCache *c = ts_calloc(1, sizeof(TSLexDeadEndCache));
    c->table = table;
    c->slots = ts_calloc(TS_LEX_DEAD_END_INIT_SLOTS, sizeof(TSLexDeadEndSlot));
    c->ends = ts_malloc(TS_LEX_DEAD_END_INIT_ENDS * sizeof(TSLexDeadEndTarget));
    c->slot_mask = TS_LEX_DEAD_END_INIT_SLOTS - 1u;
    c->end_cap = TS_LEX_DEAD_END_INIT_ENDS;
    self->dead_ends = c;
    self->utf8_fast_limit = 0;
  }
  return self->dead_ends;
}

static uint16_t ts_lex_intern_dead_end(TSLexDeadEndCache *cache, TSLexDeadEndTarget target) {
  if (cache->end_count > 0 && cache->ends[cache->end_count - 1u].pos.bytes == target.pos.bytes) {
    return (uint16_t)(cache->end_count - 1u);
  }
  if (cache->end_count == UINT16_MAX) return UINT16_MAX;
  if (cache->end_count == cache->end_cap) {
    uint32_t next_cap = (uint32_t)cache->end_cap * 2u;
    if (next_cap > UINT16_MAX) next_cap = UINT16_MAX;
    cache->ends = ts_realloc(cache->ends, (size_t)next_cap * sizeof(TSLexDeadEndTarget));
    cache->end_cap = (uint16_t)next_cap;
  }
  uint16_t idx = cache->end_count++;
  cache->ends[idx] = target;
  return idx;
}

static void ts_lex_insert_dead_slot(
  TSLexDeadEndCache *cache,
  uint16_t state,
  uint32_t byte,
  uint16_t end_idx
) {
  if (byte == UINT32_MAX) return;
  if ((cache->slot_count + 1u) * 2u > cache->slot_mask + 1u) ts_lex_grow_dead_slots(cache);
  uint32_t target = byte + 1u;
  uint32_t mask = cache->slot_mask;
  uint32_t idx = ts_lex_dead_hash(state, target) & mask;
  for (;;) {
    TSLexDeadEndSlot *slot = &cache->slots[idx];
    if (slot->byte_plus_1 == 0) {
      *slot = (TSLexDeadEndSlot){target, state, end_idx};
      cache->slot_count++;
      return;
    }
    if (slot->byte_plus_1 == target && slot->state == state) return;
    idx = (idx + 1u) & mask;
  }
}

static void ts_lex_grow_dead_slots(TSLexDeadEndCache *cache) {
  uint32_t old_cap = cache->slot_mask + 1u;
  uint32_t new_cap = old_cap * 2u;
  uint32_t new_mask = new_cap - 1u;
  TSLexDeadEndSlot *old_slots = cache->slots;
  TSLexDeadEndSlot *new_slots = ts_calloc(new_cap, sizeof(TSLexDeadEndSlot));
  for (uint32_t i = 0; i < old_cap; i++) {
    TSLexDeadEndSlot s = old_slots[i];
    if (s.byte_plus_1 == 0) continue;
    uint32_t idx = ts_lex_dead_hash(s.state, s.byte_plus_1) & new_mask;
    while (new_slots[idx].byte_plus_1 != 0) idx = (idx + 1u) & new_mask;
    new_slots[idx] = s;
  }
  ts_free(old_slots);
  cache->slots = new_slots;
  cache->slot_mask = new_mask;
}

static inline uint32_t ts_lex_dead_hash(uint16_t state, uint32_t byte_plus_1) {
  uint32_t h = (byte_plus_1 >> 6) * 0x9e3779b1u ^ ((uint32_t)state * 0x85ebca6bu) ^ byte_plus_1;
  return h ^ (h >> 16);
}
