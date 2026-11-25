#ifndef TREE_SITTER_ATOMIC_H_
#define TREE_SITTER_ATOMIC_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

#ifdef __TINYC__

static inline size_t atomic_load(const volatile size_t *p) {
  return *p;
}

static inline uint32_t atomic_inc(volatile uint32_t *p) {
  *p += 1;
  return *p;
}

static inline uint32_t atomic_dec(volatile uint32_t *p) {
  *p -= 1;
  return *p;
}

static inline uint32_t atomic_refcount_load_acquire(const volatile uint32_t *p) {
  return *p;
}

static inline uint32_t atomic_refcount_inc(volatile uint32_t *p) {
  *p += 1;
  return *p;
}

static inline bool atomic_refcount_dec_is_last(volatile uint32_t *p) {
  if (*p == 1U) return true;
  *p -= 1;
  return *p == 0U;
}

#elif defined(__ATOMIC_ACQ_REL)

static inline size_t atomic_load(const volatile size_t *p) {
  return __atomic_load_n(p, __ATOMIC_RELAXED);
}

static inline uint32_t atomic_inc(volatile uint32_t *p) {
  return __atomic_add_fetch(p, 1U, __ATOMIC_SEQ_CST);
}

static inline uint32_t atomic_dec(volatile uint32_t *p) {
  return __atomic_sub_fetch(p, 1U, __ATOMIC_SEQ_CST);
}

static inline uint32_t atomic_refcount_load_acquire(const volatile uint32_t *p) {
  return __atomic_load_n(p, __ATOMIC_ACQUIRE);
}

static inline uint32_t atomic_refcount_inc(volatile uint32_t *p) {
  return __atomic_add_fetch(p, 1U, __ATOMIC_RELAXED);
}

// When an acquire load observes `ref_count == 1`, the caller holds the sole live
// reference and any prior thread's release decrement has already synchronized.
static inline bool atomic_refcount_dec_is_last(volatile uint32_t *p) {
  if (__atomic_load_n(p, __ATOMIC_ACQUIRE) == 1U) return true;
  return __atomic_sub_fetch(p, 1U, __ATOMIC_ACQ_REL) == 0U;
}

#elif defined(_WIN32)

#include <windows.h>

static inline size_t atomic_load(const volatile size_t *p) {
  return *p;
}

static inline uint32_t atomic_inc(volatile uint32_t *p) {
  return InterlockedIncrement((long volatile *)p);
}

static inline uint32_t atomic_dec(volatile uint32_t *p) {
  return InterlockedDecrement((long volatile *)p);
}

static inline uint32_t atomic_refcount_load_acquire(const volatile uint32_t *p) {
  return (uint32_t)InterlockedOr((long volatile *)p, 0);
}

static inline uint32_t atomic_refcount_inc(volatile uint32_t *p) {
  return (uint32_t)InterlockedIncrement((long volatile *)p);
}

static inline bool atomic_refcount_dec_is_last(volatile uint32_t *p) {
  return InterlockedDecrement((long volatile *)p) == 0;
}

#else

static inline size_t atomic_load(const volatile size_t *p) {
  return __sync_fetch_and_add((volatile size_t *)p, 0);
}

static inline uint32_t atomic_inc(volatile uint32_t *p) {
  return __sync_add_and_fetch(p, 1U);
}

static inline uint32_t atomic_dec(volatile uint32_t *p) {
  return __sync_sub_and_fetch(p, 1U);
}

static inline uint32_t atomic_refcount_load_acquire(const volatile uint32_t *p) {
  return __sync_fetch_and_add((volatile uint32_t *)p, 0U);
}

static inline uint32_t atomic_refcount_inc(volatile uint32_t *p) {
  return __sync_add_and_fetch(p, 1U);
}

static inline bool atomic_refcount_dec_is_last(volatile uint32_t *p) {
  return __sync_sub_and_fetch(p, 1U) == 0U;
}

#endif

#endif  // TREE_SITTER_ATOMIC_H_
