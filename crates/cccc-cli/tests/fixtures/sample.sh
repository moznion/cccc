#!/usr/bin/env bash

# The SonarSource white-paper anchor: cognitive 7 / cyclomatic 4.
sum_of_primes() {
  local max=$1 total=0 i j
  for ((i = 2; i <= max; i++)); do
    for ((j = 2; j < i; j++)); do
      if ((i % j == 0)); then
        continue 2
      fi
    done
    ((total += i))
  done
  echo "$total"
}
